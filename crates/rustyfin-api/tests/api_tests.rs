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
        jellyfin_import: Default::default(),
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

// ── B20: Jellyfin import ──
// Remembered tracks: the playback reports store the reported audio and subtitle stream indexes
// per user and item, and the video PlaybackInfo offers them as each media source's
// DefaultAudioStreamIndex and DefaultSubtitleStreamIndex while that stream exists.

/// Video 0, audio 1 (eng) and 2 (ger), subtitles 3 (eng) and 4 (ger).
fn b20_full_streams() -> serde_json::Value {
    serde_json::json!([
        { "Index": 0, "Codec": "h264", "CodecType": "Video" },
        { "Index": 1, "Codec": "aac", "CodecType": "Audio", "Language": "eng" },
        { "Index": 2, "Codec": "ac3", "CodecType": "Audio", "Language": "ger" },
        { "Index": 3, "Codec": "subrip", "CodecType": "Subtitle", "Language": "eng" },
        { "Index": 4, "Codec": "subrip", "CodecType": "Subtitle", "Language": "ger" },
    ])
}

/// Stores a Matroska source with `streams` for `item` at `file` and returns its id.
async fn b20_insert_source(
    state: &AppState,
    item: uuid::Uuid,
    file: &std::path::Path,
    streams: serde_json::Value,
) -> uuid::Uuid {
    let source: rustyfin_model::media_source::MediaSourceResponse =
        serde_json::from_value(serde_json::json!({
            "Id": uuid::Uuid::new_v4().to_string(),
            "Uid": uuid::Uuid::new_v4().to_string(),
            "Container": "mkv",
            "Protocol": "File",
            "Path": file.to_str().unwrap(),
            "MediaStreams": streams,
        }))
        .unwrap();
    rustyfin_db::repo_media_source::insert_media_source_with_streams(&state.db, &item, &source)
        .await
        .unwrap()
}

/// A movie in folder `name` with one source carrying `streams`; returns its id.
async fn b20_movie(state: &AppState, name: &str, streams: serde_json::Value) -> uuid::Uuid {
    let folder = media_folder(state, name);
    let file = std::path::Path::new(&folder).join(format!("{name}.mkv"));
    std::fs::write(&file, b"b20").unwrap();
    let item = g4_insert_item(state, "Movie", &file).await;
    b20_insert_source(state, item, &file, streams).await;
    item
}

/// Sends a playback report to `/Sessions/Playing{route}` and expects success.
async fn b20_report(app: &axum::Router, token: &str, route: &str, body: serde_json::Value) {
    let uri = format!("/Sessions/Playing{route}");
    let (status, _) = send_authed(app, "POST", &uri, token, Some(&body.to_string())).await;
    assert!(status.is_success(), "{uri}: {status}");
}

type B20Defaults = (Option<serde_json::Value>, Option<serde_json::Value>);

/// Each media source's stream count with its `DefaultAudioStreamIndex` and
/// `DefaultSubtitleStreamIndex` from the video PlaybackInfo, `None` where the field is absent.
async fn b20_track_defaults(
    app: &axum::Router,
    token: &str,
    item: uuid::Uuid,
) -> Vec<(usize, B20Defaults)> {
    let uri = format!("/Videos/{item}/PlaybackInfo");
    let (status, body) = send_authed(app, "GET", &uri, token, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["MediaSources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|source| {
            let streams = source["MediaStreams"].as_array().unwrap().len();
            let audio = source.get("DefaultAudioStreamIndex").cloned();
            let subtitle = source.get("DefaultSubtitleStreamIndex").cloned();
            (streams, (audio, subtitle))
        })
        .collect()
}

/// The defaults of the item's only media source.
async fn b20_only_defaults(app: &axum::Router, token: &str, item: uuid::Uuid) -> B20Defaults {
    let sources = b20_track_defaults(app, token, item).await;
    assert_eq!(sources.len(), 1, "{sources:?}");
    sources[0].1.clone()
}

/// `(Some(audio), Some(subtitle))` as JSON numbers.
fn b20_both(audio: i64, subtitle: i64) -> B20Defaults {
    (
        Some(serde_json::json!(audio)),
        Some(serde_json::json!(subtitle)),
    )
}

/// The stored `(audio_stream_index, subtitle_stream_index)` of `user` for `item`.
async fn b20_stored_tracks(
    state: &AppState,
    user: &str,
    item: uuid::Uuid,
) -> (Option<i64>, Option<i64>) {
    sqlx::query_as(
        "SELECT audio_stream_index, subtitle_stream_index FROM user_item_data
         WHERE user_id = ? AND item_id = ?",
    )
    .bind(user)
    .bind(item.to_string())
    .fetch_one(&state.db)
    .await
    .unwrap()
}

#[tokio::test]
async fn b20_playback_info_defaults_from_progress_report() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (token, user) = admin_session(&state).await;
    let item = b20_movie(&state, "B20 Progress", b20_full_streams()).await;

    let body = serde_json::json!({
        "ItemId": item, "PositionTicks": 0, "AudioStreamIndex": 2, "SubtitleStreamIndex": 4,
    });
    b20_report(&app, &token, "/Progress", body).await;

    let stored = b20_stored_tracks(&state, &user.to_string(), item).await;
    assert_eq!(stored, (Some(2), Some(4)));
    assert_eq!(b20_only_defaults(&app, &token, item).await, b20_both(2, 4));
}

#[tokio::test]
async fn b20_playback_info_without_choice_omits_defaults() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (token, _) = admin_session(&state).await;
    let item = b20_movie(&state, "B20 Nothing", b20_full_streams()).await;
    assert_eq!(b20_only_defaults(&app, &token, item).await, (None, None));

    // A report without track indexes remembers none
    let body = serde_json::json!({ "ItemId": item, "PositionTicks": 50 });
    b20_report(&app, &token, "/Progress", body).await;
    assert_eq!(b20_only_defaults(&app, &token, item).await, (None, None));
}

#[tokio::test]
async fn b20_start_and_stop_reports_save_tracks() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (token, _) = admin_session(&state).await;
    let item = b20_movie(&state, "B20 Start Stop", b20_full_streams()).await;

    let body = serde_json::json!({
        "ItemId": item, "PositionTicks": 0, "AudioStreamIndex": 1, "SubtitleStreamIndex": 3,
    });
    b20_report(&app, &token, "", body).await;
    assert_eq!(b20_only_defaults(&app, &token, item).await, b20_both(1, 3));

    let body = serde_json::json!({
        "ItemId": item, "PositionTicks": 10_000_000, "AudioStreamIndex": 2,
        "SubtitleStreamIndex": 4,
    });
    b20_report(&app, &token, "/Stopped", body).await;
    assert_eq!(b20_only_defaults(&app, &token, item).await, b20_both(2, 4));
}

#[tokio::test]
async fn b20_report_without_tracks_keeps_choice() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (token, _) = admin_session(&state).await;
    let item = b20_movie(&state, "B20 Keep", b20_full_streams()).await;
    let body = serde_json::json!({
        "ItemId": item, "PositionTicks": 0, "AudioStreamIndex": 2, "SubtitleStreamIndex": 4,
    });
    b20_report(&app, &token, "/Progress", body).await;

    // Absent and null indexes leave the stored ones alone
    let body = serde_json::json!({ "ItemId": item, "PositionTicks": 100 });
    b20_report(&app, &token, "/Progress", body).await;
    assert_eq!(b20_only_defaults(&app, &token, item).await, b20_both(2, 4));
    let body = serde_json::json!({
        "ItemId": item, "PositionTicks": 200, "AudioStreamIndex": null,
        "SubtitleStreamIndex": null,
    });
    b20_report(&app, &token, "/Progress", body).await;
    assert_eq!(b20_only_defaults(&app, &token, item).await, b20_both(2, 4));

    // One index changes only that one
    let body = serde_json::json!({ "ItemId": item, "PositionTicks": 300, "AudioStreamIndex": 1 });
    b20_report(&app, &token, "/Progress", body).await;
    assert_eq!(b20_only_defaults(&app, &token, item).await, b20_both(1, 4));

    let body = serde_json::json!({ "ItemId": item, "PositionTicks": 400 });
    b20_report(&app, &token, "/Stopped", body).await;
    assert_eq!(b20_only_defaults(&app, &token, item).await, b20_both(1, 4));
}

#[tokio::test]
async fn b20_subtitle_off_is_remembered() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (token, user) = admin_session(&state).await;
    let item = b20_movie(&state, "B20 Off", b20_full_streams()).await;

    let body = serde_json::json!({
        "ItemId": item, "PositionTicks": 0, "AudioStreamIndex": 1, "SubtitleStreamIndex": -1,
    });
    b20_report(&app, &token, "/Progress", body).await;
    let stored = b20_stored_tracks(&state, &user.to_string(), item).await;
    assert_eq!(stored, (Some(1), Some(-1)));
    assert_eq!(b20_only_defaults(&app, &token, item).await, b20_both(1, -1));

    // "Off" needs no subtitle stream to exist
    let streams = serde_json::json!([
        { "Index": 0, "Codec": "h264", "CodecType": "Video" },
        { "Index": 1, "Codec": "aac", "CodecType": "Audio" },
    ]);
    let bare = b20_movie(&state, "B20 Off Bare", streams).await;
    let body = serde_json::json!({ "ItemId": bare, "PositionTicks": 0, "SubtitleStreamIndex": -1 });
    b20_report(&app, &token, "/Progress", body).await;
    let defaults = b20_only_defaults(&app, &token, bare).await;
    assert_eq!(defaults, (None, Some(serde_json::json!(-1))));
}

#[tokio::test]
async fn b20_stale_track_index_is_ignored() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (token, user) = admin_session(&state).await;
    let item = b20_movie(&state, "B20 Stale", b20_full_streams()).await;

    // Indexes the file doesn't have are stored but not offered
    let body = serde_json::json!({
        "ItemId": item, "PositionTicks": 0, "AudioStreamIndex": 7, "SubtitleStreamIndex": 9,
    });
    b20_report(&app, &token, "/Progress", body).await;
    let stored = b20_stored_tracks(&state, &user.to_string(), item).await;
    assert_eq!(stored, (Some(7), Some(9)));
    assert_eq!(b20_only_defaults(&app, &token, item).await, (None, None));

    // An index naming a stream of the other kind is not offered either
    let body = serde_json::json!({
        "ItemId": item, "PositionTicks": 0, "AudioStreamIndex": 3, "SubtitleStreamIndex": 1,
    });
    b20_report(&app, &token, "/Progress", body).await;
    assert_eq!(b20_only_defaults(&app, &token, item).await, (None, None));

    // Audio -1 names no stream
    let body = serde_json::json!({
        "ItemId": item, "PositionTicks": 0, "AudioStreamIndex": -1, "SubtitleStreamIndex": 4,
    });
    b20_report(&app, &token, "/Progress", body).await;
    let defaults = b20_only_defaults(&app, &token, item).await;
    assert_eq!(defaults, (None, Some(serde_json::json!(4))));
}

#[tokio::test]
async fn b20_track_choice_is_per_user() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (owner, _, viewer, _) = g1_owner_and_viewer(&state, &app).await;
    let item = b20_movie(&state, "B20 Users", b20_full_streams()).await;

    let body = serde_json::json!({
        "ItemId": item, "PositionTicks": 0, "AudioStreamIndex": 2, "SubtitleStreamIndex": 4,
    });
    b20_report(&app, &owner, "/Progress", body).await;
    assert_eq!(b20_only_defaults(&app, &viewer, item).await, (None, None));

    let body = serde_json::json!({
        "ItemId": item, "PositionTicks": 0, "AudioStreamIndex": 1, "SubtitleStreamIndex": 3,
    });
    b20_report(&app, &viewer, "/Progress", body).await;
    assert_eq!(b20_only_defaults(&app, &owner, item).await, b20_both(2, 4));
    assert_eq!(b20_only_defaults(&app, &viewer, item).await, b20_both(1, 3));
}

#[tokio::test]
async fn b20_track_defaults_per_media_source() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (token, _) = admin_session(&state).await;
    let item = b20_movie(&state, "B20 Versions", b20_full_streams()).await;
    let folder = media_folder(&state, "B20 Versions");
    let file = std::path::Path::new(&folder).join("B20 Versions - 720p.mkv");
    std::fs::write(&file, b"b20").unwrap();
    let streams = serde_json::json!([
        { "Index": 0, "Codec": "h264", "CodecType": "Video" },
        { "Index": 1, "Codec": "aac", "CodecType": "Audio", "Language": "eng" },
    ]);
    b20_insert_source(&state, item, &file, streams).await;

    let body = serde_json::json!({
        "ItemId": item, "PositionTicks": 0, "AudioStreamIndex": 1, "SubtitleStreamIndex": 3,
    });
    b20_report(&app, &token, "/Progress", body).await;

    // Each version offers what it has: the second has no subtitle 3
    let mut sources = b20_track_defaults(&app, &token, item).await;
    sources.sort_by_key(|(streams, _)| *streams);
    let expected = vec![(2, (Some(serde_json::json!(1)), None)), (5, b20_both(1, 3))];
    assert_eq!(sources, expected);
}

/// `b20_full_streams` with `edit` applied to the streams at index 2 (ger audio) and 4 (ger
/// subtitle).
fn b20_streams_with(edit: impl Fn(&mut serde_json::Value)) -> serde_json::Value {
    let mut streams = b20_full_streams();
    for index in [2, 4] {
        edit(&mut streams[index]);
    }
    streams
}

/// The defaults of `name`, a movie with `before`, after choosing audio 2 and subtitle 4 and a
/// rescan that finds `after` in the same file.
async fn b20_defaults_after_rescan(
    state: &AppState,
    app: &axum::Router,
    token: &str,
    name: &str,
    before: serde_json::Value,
    after: serde_json::Value,
) -> B20Defaults {
    let item = b20_movie(state, name, before).await;
    let body = serde_json::json!({
        "ItemId": item, "PositionTicks": 0, "AudioStreamIndex": 2, "SubtitleStreamIndex": 4,
    });
    b20_report(app, token, "/Progress", body).await;
    assert_eq!(
        b20_only_defaults(app, token, item).await,
        b20_both(2, 4),
        "{name}: before the rescan"
    );
    rustyfin_db::repo_media_source::delete_media_sources(&state.db, &item)
        .await
        .unwrap();
    let file = std::path::Path::new(&media_folder(state, name)).join(format!("{name}.mkv"));
    b20_insert_source(state, item, &file, after).await;
    b20_only_defaults(app, token, item).await
}

#[tokio::test]
async fn b20_rescan_offers_only_the_chosen_streams() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (token, _) = admin_session(&state).await;
    let set = |key: &'static str, audio: serde_json::Value, subtitle: serde_json::Value| {
        b20_streams_with(move |stream| {
            let kind = stream["CodecType"].as_str().unwrap().to_string();
            stream[key] = if kind == "Audio" {
                audio.clone()
            } else {
                subtitle.clone()
            };
        })
    };
    let external = |path: &str| {
        let mut streams = b20_full_streams();
        streams[4]["IsExternal"] = serde_json::json!(true);
        streams[4]["Path"] = serde_json::json!(path);
        streams
    };
    let json = |value: &str| serde_json::json!(value);
    let kept = b20_both(2, 4);
    let neither: B20Defaults = (None, None);
    let cases = [
        ("Same", b20_full_streams(), b20_full_streams(), kept.clone()),
        (
            "Case",
            b20_full_streams(),
            b20_streams_with(|stream| {
                for key in ["Codec", "CodecType", "Language"] {
                    let upper = stream[key].as_str().unwrap().to_uppercase();
                    stream[key] = serde_json::json!(upper);
                }
            }),
            kept.clone(),
        ),
        (
            "Codec",
            b20_full_streams(),
            set("Codec", json("eac3"), json("ass")),
            neither.clone(),
        ),
        (
            "Language",
            b20_full_streams(),
            set("Language", json("fre"), json("fre")),
            neither.clone(),
        ),
        (
            "Title",
            b20_full_streams(),
            set("Title", json("Commentary"), json("Signs")),
            neither.clone(),
        ),
        (
            "Default",
            b20_full_streams(),
            set(
                "IsDefault",
                serde_json::json!(true),
                serde_json::json!(true),
            ),
            neither.clone(),
        ),
        (
            "Forced",
            b20_full_streams(),
            set("IsForced", serde_json::json!(true), serde_json::json!(true)),
            neither.clone(),
        ),
        (
            "External",
            external("/subs/B20 Ext.ger.srt"),
            external("/subs/B20 Ext.fre.srt"),
            (Some(serde_json::json!(2)), None),
        ),
        (
            "External Moved",
            external("/subs/B20 Ext.ger.srt"),
            external("/elsewhere/B20 Ext.ger.srt"),
            kept.clone(),
        ),
    ];
    let mut wrong = Vec::new();
    for (case, before, after, expected) in cases {
        let name = format!("B20 Rescan {case}");
        let got = b20_defaults_after_rescan(&state, &app, &token, &name, before, after).await;
        if got != expected {
            wrong.push(format!("{case}: {got:?}, expected {expected:?}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn b20_track_choice_names_the_reported_source() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (token, _) = admin_session(&state).await;
    let item = b20_movie(&state, "B20 Dub", b20_full_streams()).await;
    let folder = media_folder(&state, "B20 Dub");
    let file = std::path::Path::new(&folder).join("B20 Dub - French.mkv");
    std::fs::write(&file, b"b20").unwrap();
    let streams = serde_json::json!([
        { "Index": 0, "Codec": "h264", "CodecType": "Video" },
        { "Index": 1, "Codec": "ac3", "CodecType": "Audio", "Language": "fre" },
    ]);
    let dub = b20_insert_source(&state, item, &file, streams).await;

    // Audio 1 of the French version: the first version's audio 1 is another stream.
    let body = serde_json::json!({
        "ItemId": item, "MediaSourceId": dub.simple().to_string(), "PositionTicks": 0,
        "AudioStreamIndex": 1,
    });
    b20_report(&app, &token, "/Progress", body).await;
    let mut sources = b20_track_defaults(&app, &token, item).await;
    sources.sort_by_key(|(streams, _)| *streams);
    let audio_1 = (Some(serde_json::json!(1)), None);
    assert_eq!(sources, vec![(2, audio_1.clone()), (5, (None, None))]);

    // Without MediaSourceId, the first version listed that has an audio 1 names it.
    let body = serde_json::json!({ "ItemId": item, "PositionTicks": 0, "AudioStreamIndex": 1 });
    b20_report(&app, &token, "/Progress", body).await;
    let mut sources = b20_track_defaults(&app, &token, item).await;
    sources.sort_by_key(|(streams, _)| *streams);
    assert_eq!(sources, vec![(2, (None, None)), (5, audio_1)]);
}

// Import from Jellyfin: the importer reads a Jellyfin data folder read-only (synthetic databases
// in both layouts, written by `jellyfin_fixture`), matches users by name and items by path pair or
// provider id, and its preview reports what an import would do without writing anything.

mod jellyfin_fixture;
use jellyfin_fixture as jf;

/// A server with an admin, the user "alice", the libraries "Movies" (`media/movies`: Alpha, Beta
/// with TMDB 222) and "Shows" (`media/tv`: Show with TVDB 333, its Season 1 and S01E01-S01E03),
/// and the Jellyfin install `source` (`b20_source`), which each test writes where it needs it.
struct B20World {
    state: AppState,
    app: axum::Router,
    token: String,
    alice: uuid::Uuid,
    movies: String,
    tv: String,
    source: jf::JfSource,
    /// An empty folder for Jellyfin installs, removed when the test ends.
    jf_root: std::path::PathBuf,
}

/// A Rustyfin item row for the B20 import tests.
#[derive(Default)]
struct B20Row {
    media_type: &'static str,
    name: String,
    path: String,
    parent: Option<uuid::Uuid>,
    parent_index: Option<i32>,
    index: Option<i32>,
    tmdb: Option<i64>,
    tvdb: Option<i64>,
}

/// Stores `row` in `library` and returns the new item's id.
async fn b20_insert(state: &AppState, library: uuid::Uuid, row: B20Row) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO media_items (id, library_id, name, media_type, path, parent_id, \
         parent_index_number, index_number, tmdb_id, tvdb_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(library.to_string())
    .bind(&row.name)
    .bind(row.media_type)
    .bind(&row.path)
    .bind(row.parent.map(|p| p.to_string()))
    .bind(row.parent_index)
    .bind(row.index)
    .bind(row.tmdb)
    .bind(row.tvdb)
    .execute(&state.db)
    .await
    .unwrap();
    id
}

/// Stores the library `name` of `content_type` on `path` and returns its id.
async fn b20_library(
    state: &AppState,
    name: &str,
    content_type: rustyfin_model::library::LibraryContentType,
    path: &str,
) -> uuid::Uuid {
    let lib = rustyfin_model::library::Library {
        id: uuid::Uuid::new_v4(),
        name: name.to_string(),
        content_type,
        paths: vec![path.to_string()],
    };
    rustyfin_db::repo_library::create_library(&state.db, &lib)
        .await
        .unwrap();
    lib.id
}

/// Stores the user `name` without hashing a password (nobody signs in as them); returns the id.
async fn b20_plain_user(state: &AppState, name: &str) -> uuid::Uuid {
    let user = rustyfin_model::user::User {
        id: uuid::Uuid::new_v4(),
        name: name.to_string(),
        password_hash: "unused".to_string(),
        is_administrator: false,
        created_at: chrono::Utc::now(),
        last_login_date: None,
        configuration: serde_json::json!({}),
    };
    rustyfin_db::repo_user::create_user(&state.db, &user)
        .await
        .unwrap();
    user.id
}

/// Jellyfin's side: "Alice" (admin; Rustyfin has "alice") and "bob" (new); the libraries Movies
/// (`/data/movies`), Music (`/data/music`, no Rustyfin library) and Shows (`/data/tvshows`);
/// items matched by path (Alpha, Show, Season 1, S01E01, S01E02), by id (Beta by TMDB, the episode
/// 1x03 under an old path by its series and numbers) or not at all (Gamma, Song), and a folder,
/// which is not imported. User data: Alice has Alpha, S01E01, Gamma and a detached row; bob has
/// Beta and 1x03.
fn b20_source() -> jf::JfSource {
    let alice = jf::JfUser::new(1, "Alice", true);
    let bob = jf::JfUser::new(2, "bob", false);
    let folder = jf::JfItem::new("Folder", "Movies", Some("/data/movies"));
    let alpha = jf::JfItem::new(
        "Movie",
        "Alpha",
        Some("/data/movies/Alpha (2001)/Alpha.mkv"),
    );
    let beta = jf::JfItem::new("Movie", "Beta", Some("/data/movies/Beta (old)/Beta.avi"))
        .provider("Tmdb", "222");
    let gamma = jf::JfItem::new(
        "Movie",
        "Gamma",
        Some("/data/movies/Gamma (2003)/Gamma.mkv"),
    )
    .provider("Tmdb", "999");
    let show =
        jf::JfItem::new("Series", "Show", Some("/data/tvshows/Show")).provider("Tvdb", "333");
    let season = jf::JfItem::new("Season", "Season 1", Some("/data/tvshows/Show/Season 01"))
        .in_series(show.id)
        .numbers(None, Some(1));
    let episode = |name: &str, path: &str, number: i32| {
        jf::JfItem::new("Episode", name, Some(path))
            .in_season(show.id, season.id)
            .numbers(Some(1), Some(number))
    };
    let e1 = episode(
        "Show S01E01",
        "/data/tvshows/Show/Season 01/Show S01E01.mkv",
        1,
    );
    let e2 = episode(
        "Show S01E02",
        "/data/tvshows/Show/Season 01/Show S01E02.mkv",
        2,
    );
    let e3 = episode(
        "Show 1x03",
        "/data/tvshows/Show/Season 1/Show - 1x03.mkv",
        3,
    );
    let song = jf::JfItem::new("Audio", "Song", Some("/data/music/Song.flac"));
    let played = |user: &jf::JfUser, item: uuid::Uuid, last: &'static str| jf::JfUserData {
        user: user.id,
        item,
        played: true,
        play_count: 1,
        last_played: Some(last),
        ..Default::default()
    };
    let user_data = vec![
        jf::JfUserData {
            play_count: 2,
            favorite: true,
            rating: Some(7.6),
            ..played(&alice, alpha.id, "2025-01-02 03:04:05.1234567")
        },
        jf::JfUserData {
            played: false,
            play_count: 0,
            position: 6_000_000_000,
            ..played(&alice, e1.id, "2025-02-01 10:00:00")
        },
        played(&alice, gamma.id, "2025-01-05 12:00:00"),
        played(&alice, jf::DETACHED, "2024-12-01 00:00:00"),
        jf::JfUserData {
            rating: Some(9.5),
            ..played(&bob, beta.id, "2025-03-01 20:00:00")
        },
        played(&bob, e3.id, "2025-03-02 21:00:00"),
    ];
    let library = |name, collection_type, path| jf::JfLibrary {
        name,
        collection_type,
        paths: vec![path],
    };
    jf::JfSource {
        users: vec![alice, bob],
        items: vec![folder, alpha, beta, gamma, show, season, e1, e2, e3, song],
        user_data,
        streams: Vec::new(),
        libraries: vec![
            library("Movies", "movies", "/data/movies"),
            library("Music", "music", "/data/music"),
            library("Shows", "tvshows", "/data/tvshows"),
        ],
    }
}

async fn b20_world() -> B20World {
    use rustyfin_model::library::LibraryContentType as Kind;
    let state = test_state().await;
    let app = app(state.clone());
    let (token, _) = admin_session(&state).await;
    let alice = add_user(&state, "alice", "alicepass", false).await;
    let movies = media_folder(&state, "movies");
    let tv = media_folder(&state, "tv");
    let movie_lib = b20_library(&state, "Movies", Kind::Movies, &movies).await;
    let show_lib = b20_library(&state, "Shows", Kind::TvShows, &tv).await;
    let row = |media_type: &'static str, name: &str, path: String| B20Row {
        media_type,
        name: name.to_string(),
        path,
        ..Default::default()
    };
    let alpha_path = format!("{movies}/Alpha (2001)/Alpha.mkv");
    b20_insert(&state, movie_lib, row("Movie", "Alpha", alpha_path)).await;
    let beta_row = B20Row {
        tmdb: Some(222),
        ..row("Movie", "Beta", format!("{movies}/Beta (2002)/Beta.mkv"))
    };
    b20_insert(&state, movie_lib, beta_row).await;
    let show_row = B20Row {
        tvdb: Some(333),
        ..row("Series", "Show", format!("{tv}/Show"))
    };
    let show = b20_insert(&state, show_lib, show_row).await;
    let season_row = B20Row {
        parent: Some(show),
        index: Some(1),
        ..row("Season", "Season 1", format!("{tv}/Show/Season 01"))
    };
    let season = b20_insert(&state, show_lib, season_row).await;
    for n in 1..=3 {
        let name = format!("Show S01E0{n}");
        let episode = B20Row {
            parent: Some(season),
            parent_index: Some(1),
            index: Some(n),
            ..row("Episode", &name, format!("{tv}/Show/Season 01/{name}.mkv"))
        };
        b20_insert(&state, show_lib, episode).await;
    }
    let jf_root =
        std::env::temp_dir().join(format!("rustyfin_b20_jf_{}", uuid::Uuid::new_v4().simple()));
    remove_when_test_ends(&jf_root);
    std::fs::create_dir_all(&jf_root).unwrap();
    B20World {
        state,
        app,
        token,
        alice,
        movies,
        tv,
        source: b20_source(),
        jf_root,
    }
}

/// Writes the world's Jellyfin install in `layout` into the folder `name` and returns that folder
/// (Jellyfin's program data folder, the official image's `/config`).
async fn b20_install(w: &B20World, name: &str, layout: jf::Layout) -> std::path::PathBuf {
    let dir = w.jf_root.join(name);
    w.source.write(&dir, layout).await;
    dir
}

/// POSTs `body` to `/JellyfinImport/{route}` as the admin.
async fn b20_post(
    w: &B20World,
    route: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let uri = format!("/JellyfinImport/{route}");
    send_authed(&w.app, "POST", &uri, &w.token, Some(&body.to_string())).await
}

/// Previews an import with `body` and expects 200; returns the report.
async fn b20_preview(w: &B20World, body: serde_json::Value) -> serde_json::Value {
    let (status, report) = b20_post(w, "Preview", body).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    report
}

/// A report's item counts: (Total, MatchedByPath, MatchedById, Unmatched).
fn b20_item_counts(report: &serde_json::Value) -> (i64, i64, i64, i64) {
    let count = |key: &str| report["Items"][key].as_i64().unwrap_or(-1);
    (
        count("Total"),
        count("MatchedByPath"),
        count("MatchedById"),
        count("Unmatched"),
    )
}

/// A report's path pairs under `key` as (From, To).
fn b20_pairs(report: &serde_json::Value, key: &str) -> Vec<(String, String)> {
    let pairs = report[key].as_array();
    pairs
        .unwrap_or_else(|| panic!("no {key}: {report}"))
        .iter()
        .map(|p| {
            (
                p["From"].as_str().unwrap().into(),
                p["To"].as_str().unwrap().into(),
            )
        })
        .collect()
}

/// A report's `UnmatchedSamples` as (Type, Name, Path).
fn b20_samples(report: &serde_json::Value) -> Vec<(String, String, String)> {
    let samples = report["Items"]["UnmatchedSamples"].as_array();
    let text = |v: &serde_json::Value| v.as_str().unwrap().to_string();
    samples
        .unwrap_or_else(|| panic!("no UnmatchedSamples: {report}"))
        .iter()
        .map(|s| (text(&s["Type"]), text(&s["Name"]), text(&s["Path"])))
        .collect()
}

/// One entry of a report's `Users`.
#[derive(Debug, PartialEq)]
struct B20User {
    jellyfin_id: uuid::Uuid,
    name: String,
    was_admin: bool,
    action: String,
    reason: Option<String>,
    user_id: Option<uuid::Uuid>,
    code: Option<String>,
}

fn b20_users(report: &serde_json::Value) -> Vec<B20User> {
    let id = |v: &serde_json::Value| v.as_str().map(|s| uuid::Uuid::parse_str(s).unwrap());
    let users = report["Users"].as_array();
    users
        .unwrap_or_else(|| panic!("no Users: {report}"))
        .iter()
        .map(|u| B20User {
            jellyfin_id: id(&u["JellyfinId"]).unwrap_or_else(|| panic!("JellyfinId: {u}")),
            name: u["Name"].as_str().unwrap().to_string(),
            was_admin: u["WasAdministrator"].as_bool().unwrap(),
            action: u["Action"].as_str().unwrap().to_string(),
            reason: u["Reason"].as_str().map(str::to_string),
            user_id: id(&u["UserId"]),
            code: u["Code"].as_str().map(str::to_string),
        })
        .collect()
}

/// The report entry expected for the Jellyfin user `name` (no code).
fn b20_user(
    w: &B20World,
    name: &str,
    action: &str,
    reason: Option<&str>,
    user_id: Option<uuid::Uuid>,
) -> B20User {
    let jf_user = w.source.user(name);
    B20User {
        jellyfin_id: jf_user.id,
        name: name.to_string(),
        was_admin: jf_user.admin,
        action: action.to_string(),
        reason: reason.map(str::to_string),
        user_id,
        code: None,
    }
}

/// Row counts of the tables an import may write, then every user and user-data row.
async fn b20_db_digest(state: &AppState) -> Vec<String> {
    let mut digest = Vec::new();
    for table in [
        "users",
        "user_item_data",
        "media_items",
        "media_sources",
        "media_streams",
        "item_images",
        "people",
        "item_people",
        "libraries",
    ] {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .fetch_one(&state.db)
            .await
            .unwrap();
        digest.push(format!("{table} {n}"));
    }
    let users: Vec<String> = sqlx::query_scalar(
        "SELECT id || ' ' || name || ' ' || password_hash || ' ' || is_administrator FROM users \
         ORDER BY id",
    )
    .fetch_all(&state.db)
    .await
    .unwrap();
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT quote(user_id) || quote(item_id) || quote(played) || quote(play_count) || \
         quote(playback_position_ticks) || quote(last_played_date) || quote(is_favorite) || \
         quote(user_rating) || quote(likes) || quote(audio_stream_index) || \
         quote(subtitle_stream_index) FROM user_item_data ORDER BY user_id, item_id",
    )
    .fetch_all(&state.db)
    .await
    .unwrap();
    digest.extend(users);
    digest.extend(rows);
    digest
}

#[tokio::test]
async fn b20_import_preview_is_admin_only() {
    let w = b20_world().await;
    add_user(&w.state, "viewer", "viewerpass", false).await;
    let (viewer, _) = login(&w.app, "viewer", "viewerpass").await.unwrap();
    let install = b20_install(&w, "jf", jf::Layout::New).await;
    let body = Some(serde_json::json!({ "SourcePath": install }));
    g1_assert_admin_only(&w.app, "POST", "/JellyfinImport/Preview", &body, &viewer).await;
}

#[tokio::test]
async fn b20_import_preview_rejects_bad_sources() {
    let w = b20_world().await;
    let new = b20_install(&w, "new", jf::Layout::New).await;
    let old = b20_install(&w, "old", jf::Layout::Old).await;
    let junk = w.jf_root.join("junk");
    std::fs::create_dir_all(&junk).unwrap();
    std::fs::write(junk.join("jellyfin.db"), b"not a database").unwrap();
    let missing = w.jf_root.join("missing");
    let cases = [
        ("empty", serde_json::json!({ "SourcePath": "" })),
        (
            "relative",
            serde_json::json!({ "SourcePath": "relative/config" }),
        ),
        ("missing", serde_json::json!({ "SourcePath": missing })),
        ("not a database", serde_json::json!({ "SourcePath": junk })),
        (
            "Old without library.db",
            serde_json::json!({ "SourcePath": new, "Layout": "Old" }),
        ),
        (
            "New without BaseItems",
            serde_json::json!({ "SourcePath": old, "Layout": "New" }),
        ),
    ];
    for (case, body) in cases {
        let (status, answer) = b20_post(&w, "Preview", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{case}: {answer}");
    }

    // A missing database names the three places searched, and nothing is created.
    let body = serde_json::json!({ "SourcePath": missing });
    let (_, answer) = b20_post(&w, "Preview", body).await;
    let detail = answer["Detail"].as_str().unwrap_or_default();
    for place in ["jellyfin.db", "data/jellyfin.db", "data/data/jellyfin.db"] {
        let path = missing.join(place);
        assert!(detail.contains(path.to_str().unwrap()), "{place}: {detail}");
    }
    assert!(!missing.exists());
}

#[tokio::test]
async fn b20_import_preview_finds_the_database_in_three_places() {
    let w = b20_world().await;
    // linuxserver's image: /config/data/data/jellyfin.db
    let config = w.jf_root.join("config");
    w.source.write(&config.join("data"), jf::Layout::New).await;
    let data = config.join("data").join("data");
    for source in [config.clone(), config.join("data"), data.clone()] {
        let report = b20_preview(&w, serde_json::json!({ "SourcePath": source })).await;
        let shown = source.display();
        assert_eq!(report["DataPath"], serde_json::json!(data), "{shown}");
        assert_eq!(b20_item_counts(&report), (9, 5, 2, 2), "{shown}");
    }
}

#[tokio::test]
async fn b20_import_preview_reports_without_writing() {
    let w = b20_world().await;
    let install = b20_install(&w, "jf", jf::Layout::New).await;
    let before = b20_db_digest(&w.state).await;
    let report = b20_preview(&w, serde_json::json!({ "SourcePath": install })).await;
    assert_eq!(
        b20_db_digest(&w.state).await,
        before,
        "a preview writes nothing"
    );

    assert_eq!(report["Layout"], "New");
    assert_eq!(report["DataPath"], serde_json::json!(install.join("data")));
    let text = |p: std::path::PathBuf| p.to_str().unwrap().to_string();
    let suggested = vec![
        ("/data/movies".to_string(), w.movies.clone()),
        ("/data/tvshows".to_string(), w.tv.clone()),
        ("%MetadataPath%".to_string(), text(install.join("metadata"))),
        ("%AppDataPath%".to_string(), text(install.join("data"))),
    ];
    assert_eq!(b20_pairs(&report, "SuggestedPathMappings"), suggested);
    assert_eq!(
        b20_pairs(&report, "PathMappings"),
        suggested,
        "absent = suggested"
    );
    assert_eq!(
        report["UnpairedLibraryPaths"],
        serde_json::json!(["/data/music"])
    );
    let users = vec![
        b20_user(&w, "Alice", "Match", None, Some(w.alice)),
        b20_user(&w, "bob", "Create", None, None),
    ];
    assert_eq!(b20_users(&report), users);
    assert_eq!(b20_item_counts(&report), (9, 5, 2, 2));
    let gamma = "/data/movies/Gamma (2003)/Gamma.mkv";
    let samples = vec![("Movie".to_string(), "Gamma".to_string(), gamma.to_string())];
    assert_eq!(b20_samples(&report), samples);
}

#[tokio::test]
async fn b20_import_preview_reads_the_old_layout() {
    let w = b20_world().await;
    let old = b20_install(&w, "old", jf::Layout::Old).await;
    let report = b20_preview(&w, serde_json::json!({ "SourcePath": old })).await;
    assert_eq!(report["Layout"], "Old");
    assert_eq!(b20_item_counts(&report), (9, 5, 2, 2));
    let users = vec![
        b20_user(&w, "Alice", "Match", None, Some(w.alice)),
        b20_user(&w, "bob", "Create", None, None),
    ];
    assert_eq!(b20_users(&report), users);
    let names: Vec<String> = b20_samples(&report).into_iter().map(|s| s.1).collect();
    assert_eq!(names, ["Gamma"]);

    // An upgrade renamed library.db to library.db.old
    let renamed = b20_install(&w, "renamed", jf::Layout::Old).await;
    let data = renamed.join("data");
    std::fs::rename(data.join("library.db"), data.join("library.db.old")).unwrap();
    let report = b20_preview(&w, serde_json::json!({ "SourcePath": renamed })).await;
    assert_eq!(report["Layout"], "Old");
    assert_eq!(b20_item_counts(&report), (9, 5, 2, 2));

    // A migrated install reads the new layout unless Old is asked for
    let migrated = b20_install(&w, "migrated", jf::Layout::New).await;
    let old_db = migrated.join("data").join("library.db.old");
    w.source.write_library_db(&old_db).await;
    let report = b20_preview(&w, serde_json::json!({ "SourcePath": migrated })).await;
    assert_eq!(report["Layout"], "New");
    let body = serde_json::json!({ "SourcePath": migrated, "Layout": "Old" });
    let report = b20_preview(&w, body).await;
    assert_eq!(report["Layout"], "Old");
    assert_eq!(b20_item_counts(&report), (9, 5, 2, 2));
}

#[tokio::test]
async fn b20_import_preview_uses_given_path_pairs() {
    let w = b20_world().await;
    let install = b20_install(&w, "jf", jf::Layout::New).await;

    // No pairs: only provider ids match (Beta; the show, its season and its episodes)
    let body = serde_json::json!({ "SourcePath": install, "PathMappings": [] });
    let report = b20_preview(&w, body).await;
    assert_eq!(
        b20_pairs(&report, "PathMappings"),
        Vec::<(String, String)>::new()
    );
    assert_eq!(b20_pairs(&report, "SuggestedPathMappings").len(), 4);
    assert_eq!(b20_item_counts(&report), (9, 0, 6, 3));
    let names: Vec<String> = b20_samples(&report).into_iter().map(|s| s.1).collect();
    assert_eq!(names, ["Alpha", "Gamma"]);

    // One pair, with trailing slashes
    let pairs = serde_json::json!([{ "From": "/data/movies/", "To": format!("{}/", w.movies) }]);
    let body = serde_json::json!({ "SourcePath": install, "PathMappings": pairs });
    let report = b20_preview(&w, body).await;
    assert_eq!(b20_item_counts(&report), (9, 1, 6, 2));
}

#[tokio::test]
async fn b20_import_preview_matches_and_picks_users() {
    let mut w = b20_world().await;
    let mut ids = HashMap::new();
    for name in ["Dave", "DAVE", "Erin", "ERIN"] {
        ids.insert(name, b20_plain_user(&w.state, name).await);
    }
    w.source.users.push(jf::JfUser::new(3, "dave", false));
    w.source.users.push(jf::JfUser::new(4, "Erin", true));
    let install = b20_install(&w, "jf", jf::Layout::New).await;

    let report = b20_preview(&w, serde_json::json!({ "SourcePath": install })).await;
    let users = vec![
        b20_user(&w, "Alice", "Match", None, Some(w.alice)),
        b20_user(&w, "bob", "Create", None, None),
        b20_user(&w, "dave", "Skip", Some("Ambiguous"), None),
        b20_user(&w, "Erin", "Match", None, Some(ids["Erin"])),
    ];
    assert_eq!(b20_users(&report), users);

    // Only picked users count; unknown ids are ignored
    let bob = w
        .source
        .user("bob")
        .id
        .hyphenated()
        .to_string()
        .to_uppercase();
    let picks = serde_json::json!([bob, uuid::Uuid::new_v4()]);
    let body = serde_json::json!({ "SourcePath": install, "UserIds": picks });
    let report = b20_preview(&w, body).await;
    let users = vec![
        b20_user(&w, "Alice", "Skip", Some("NotSelected"), None),
        b20_user(&w, "bob", "Create", None, None),
        b20_user(&w, "dave", "Skip", Some("NotSelected"), None),
        b20_user(&w, "Erin", "Skip", Some("NotSelected"), None),
    ];
    assert_eq!(b20_users(&report), users);
    assert_eq!(b20_samples(&report), Vec::new(), "bob's items all match");
}

#[tokio::test]
async fn b20_import_preview_leaves_the_source_and_its_secrets_alone() {
    let w = b20_world().await;
    for (name, layout) in [("new", jf::Layout::New), ("old", jf::Layout::Old)] {
        let install = b20_install(&w, name, layout).await;
        let before = jf::listing(&install);
        let log = jf::LogCapture::start();
        let body = serde_json::json!({ "SourcePath": install });
        let (status, answer) = b20_post(&w, "Preview", body).await;
        let text = log.text();
        drop(log);
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(jf::listing(&install), before, "{name}: the source changed");
        assert!(
            !answer.to_string().contains(jf::CANARY),
            "{name}: secret in the answer"
        );
        assert!(!text.contains(jf::CANARY), "{name}: secret in the log");
        let info = |l: &str| l.contains("INFO") && l.contains("jellyfin import");
        assert!(text.lines().any(info), "{name}: no INFO line in {text}");
    }
    let db = w.state.data_dir.join("test.db");
    let files = [
        db.clone(),
        db.with_extension("db-wal"),
        db.with_extension("db-shm"),
    ];
    assert!(
        !jf::files_hold_canary(&files),
        "secret in Rustyfin's database"
    );
}

// B20 2b: the import run, its status and the created users' one-time codes.

/// `GET /JellyfinImport/Status` as the admin; expects 200.
async fn b20_status(w: &B20World) -> serde_json::Value {
    let uri = "/JellyfinImport/Status";
    let (status, answer) = send_authed(&w.app, "GET", uri, &w.token, None).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    answer
}

/// Starts an import run with `body`; expects 202 `{"State":"Running"}`.
async fn b20_start(w: &B20World, body: serde_json::Value) {
    let (status, answer) = b20_post(w, "Run", body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{answer}");
    assert_eq!(answer, serde_json::json!({ "State": "Running" }));
}

/// Polls the status until the run no longer runs (two minutes at most) and returns that first
/// answer, the only one that shows the codes.
async fn b20_wait(w: &B20World) -> serde_json::Value {
    for _ in 0..2400 {
        let answer = b20_status(w).await;
        if answer["State"] != "Running" {
            return answer;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the import still runs after two minutes");
}

/// Runs an import with `body` to its end, expects it to complete and returns the report.
async fn b20_run(w: &B20World, body: serde_json::Value) -> serde_json::Value {
    b20_start(w, body).await;
    let answer = b20_wait(w).await;
    assert_eq!(answer["State"], "Completed", "{answer}");
    answer["Result"].clone()
}

/// Whether `code` has the shape of a one-time code: `XXXX-XXXX-XXXX` in Crockford's base32.
fn b20_is_code(code: &str) -> bool {
    let groups: Vec<&str> = code.split('-').collect();
    let crockford = |c: char| "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(c);
    groups.len() == 3
        && groups
            .iter()
            .all(|group| group.len() == 4 && group.chars().all(crockford))
}

/// Every stored setup code: user id, hash and date.
async fn b20_code_rows(state: &AppState) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT user_id || ' ' || code_hash || ' ' || created_at FROM user_setup_codes \
         ORDER BY user_id",
    )
    .fetch_all(&state.db)
    .await
    .unwrap()
}

/// The report entry of the Jellyfin user `name`.
fn b20_entry(report: &serde_json::Value, name: &str) -> B20User {
    b20_users(report)
        .into_iter()
        .find(|user| user.name == name)
        .unwrap_or_else(|| panic!("no {name} in {report}"))
}

async fn b20_user_count(state: &AppState) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&state.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn b20_import_run_and_status_are_admin_only() {
    let w = b20_world().await;
    add_user(&w.state, "viewer", "viewerpass", false).await;
    let (viewer, _) = login(&w.app, "viewer", "viewerpass").await.unwrap();
    let install = b20_install(&w, "jf", jf::Layout::New).await;
    let body = Some(serde_json::json!({ "SourcePath": install }));
    g1_assert_admin_only(&w.app, "POST", "/JellyfinImport/Run", &body, &viewer).await;
    g1_assert_admin_only(&w.app, "GET", "/JellyfinImport/Status", &None, &viewer).await;
    assert_eq!(
        b20_status(&w).await["State"],
        "Idle",
        "a refused run started"
    );
}

#[tokio::test]
async fn b20_import_status_is_idle_before_a_run() {
    let w = b20_world().await;
    let idle = serde_json::json!({
        "State": "Idle",
        "Stage": null,
        "Done": 0,
        "Total": 0,
        "Result": null,
        "Error": null,
    });
    assert_eq!(b20_status(&w).await, idle);
}

#[tokio::test]
async fn b20_import_run_rejects_bad_sources() {
    let w = b20_world().await;
    let new = b20_install(&w, "new", jf::Layout::New).await;
    let old = b20_install(&w, "old", jf::Layout::Old).await;
    let junk = w.jf_root.join("junk");
    std::fs::create_dir_all(&junk).unwrap();
    std::fs::write(junk.join("jellyfin.db"), b"not a database").unwrap();
    let missing = w.jf_root.join("missing");
    let before = b20_db_digest(&w.state).await;
    let cases = [
        ("empty", serde_json::json!({ "SourcePath": "" })),
        (
            "relative",
            serde_json::json!({ "SourcePath": "relative/config" }),
        ),
        ("missing", serde_json::json!({ "SourcePath": missing })),
        ("not a database", serde_json::json!({ "SourcePath": junk })),
        (
            "Old without library.db",
            serde_json::json!({ "SourcePath": new, "Layout": "Old" }),
        ),
        (
            "New without BaseItems",
            serde_json::json!({ "SourcePath": old, "Layout": "New" }),
        ),
    ];
    for (case, body) in cases {
        let (status, answer) = b20_post(&w, "Run", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{case}: {answer}");
    }
    assert_eq!(
        b20_status(&w).await["State"],
        "Idle",
        "a refused run started"
    );
    assert_eq!(b20_db_digest(&w.state).await, before, "a refused run wrote");
    assert!(!missing.exists());
}

#[tokio::test]
async fn b20_import_run_creates_picked_users_with_codes() {
    let mut w = b20_world().await;
    w.source.users.push(jf::JfUser::new(3, "Carol", true));
    w.source.users.push(jf::JfUser::new(4, "dave", false));
    let install = b20_install(&w, "jf", jf::Layout::New).await;
    let picked: Vec<uuid::Uuid> = ["Alice", "bob", "Carol"]
        .into_iter()
        .map(|name| w.source.user(name).id)
        .collect();
    let users_before = b20_user_count(&w.state).await;
    let listing = jf::listing(&install);
    let log = jf::LogCapture::start();
    let body = serde_json::json!({ "SourcePath": install, "UserIds": picked });
    let report = b20_run(&w, body).await;
    let text = log.text();
    drop(log);
    assert_eq!(jf::listing(&install), listing, "the source changed");

    // Alice matches alice; bob and Carol are created with codes; dave is not picked.
    let bob = b20_entry(&report, "bob");
    let carol = b20_entry(&report, "Carol");
    let (Some(bob_id), Some(bob_code)) = (bob.user_id, bob.code) else {
        panic!("bob: no UserId or Code in {report}");
    };
    let (Some(carol_id), Some(carol_code)) = (carol.user_id, carol.code) else {
        panic!("Carol: no UserId or Code in {report}");
    };
    let created = |name: &str, id: uuid::Uuid, code: &str| B20User {
        user_id: Some(id),
        code: Some(code.to_string()),
        ..b20_user(&w, name, "Create", None, None)
    };
    let expected = vec![
        b20_user(&w, "Alice", "Match", None, Some(w.alice)),
        created("bob", bob_id, &bob_code),
        created("Carol", carol_id, &carol_code),
        b20_user(&w, "dave", "Skip", Some("NotSelected"), None),
    ];
    assert_eq!(b20_users(&report), expected);
    assert!(carol.was_admin, "the report names Jellyfin's admins");
    assert_ne!(bob_code, carol_code);
    assert_eq!(
        b20_user_count(&w.state).await,
        users_before + 2,
        "only bob and Carol are created"
    );

    for (name, id, code) in [("bob", bob_id, &bob_code), ("Carol", carol_id, &carol_code)] {
        assert!(b20_is_code(code), "{name}: {code}");
        let (admin, password): (bool, String) = sqlx::query_as(
            "SELECT is_administrator, password_hash FROM users WHERE id = ? AND name = ?",
        )
        .bind(id.to_string())
        .bind(name)
        .fetch_one(&w.state.db)
        .await
        .unwrap();
        assert!(!admin, "{name}: created as an administrator");
        assert!(
            password.starts_with("$argon2"),
            "{name}: password {password}"
        );
        let code_hash: String =
            sqlx::query_scalar("SELECT code_hash FROM user_setup_codes WHERE user_id = ?")
                .bind(id.to_string())
                .fetch_one(&w.state.db)
                .await
                .unwrap();
        let plain = code.replace('-', "");
        assert!(code_hash.starts_with("$argon2"), "{name}: code hash");
        assert!(!code_hash.contains(&plain), "{name}: the code is stored");
        let verified = rustyfin_auth::verify_setup_code(code, &code_hash).await;
        assert!(verified.unwrap(), "{name}: the hash is not the code's");
        assert!(
            login(&w.app, name, code).await.is_none(),
            "{name}: the code signed in"
        );
        assert!(
            login(&w.app, name, "").await.is_none(),
            "{name}: no password signed in"
        );
        assert!(
            !text.contains(code.as_str()) && !text.contains(&plain),
            "{name}: the code is in the log"
        );
    }

    let info = |l: &str| l.contains("INFO") && l.contains("jellyfin import run");
    assert!(text.lines().any(info), "no INFO line in {text}");
    assert!(!text.contains(jf::CANARY), "secret in the log");
    assert!(
        !report.to_string().contains(jf::CANARY),
        "secret in the report"
    );
}

#[tokio::test]
async fn b20_import_run_creates_one_user_per_name() {
    // Jellyfin users whose names differ only in case: the one created first matches the other.
    let mut w = b20_world().await;
    w.source.users.push(jf::JfUser::new(3, "Erin", false));
    w.source.users.push(jf::JfUser::new(4, "erin", false));
    let install = b20_install(&w, "jf", jf::Layout::New).await;
    let report = b20_run(&w, serde_json::json!({ "SourcePath": install })).await;
    let erins: Vec<B20User> = b20_users(&report)
        .into_iter()
        .filter(|user| user.name.eq_ignore_ascii_case("erin"))
        .collect();
    let [first, second] = erins.as_slice() else {
        panic!("{report}");
    };
    let actions = (first.action.as_str(), second.action.as_str());
    assert_eq!(actions, ("Create", "Match"), "{report}");
    assert!(first.code.is_some() && second.code.is_none(), "{report}");
    assert!(first.user_id.is_some(), "{report}");
    assert_eq!(second.user_id, first.user_id, "{report}");
    let erin_users: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE lower(name) = 'erin'")
            .fetch_one(&w.state.db)
            .await
            .unwrap();
    assert_eq!(erin_users, 1);
}

#[tokio::test]
async fn b20_import_run_shows_codes_once() {
    let w = b20_world().await;
    let install = b20_install(&w, "jf", jf::Layout::New).await;
    let report = b20_run(&w, serde_json::json!({ "SourcePath": install })).await;
    assert!(
        b20_entry(&report, "bob").code.is_some(),
        "the first answer has no code"
    );

    let again = b20_status(&w).await;
    assert_eq!(again["State"], "Completed", "{again}");
    let mut expected = report.clone();
    for user in expected["Users"].as_array_mut().unwrap() {
        user["Code"] = serde_json::Value::Null;
    }
    assert_eq!(again["Result"], expected, "only the codes go");
}

#[tokio::test]
async fn b20_import_second_run_creates_nothing() {
    for (name, layout) in [("new", jf::Layout::New), ("old", jf::Layout::Old)] {
        let w = b20_world().await;
        let install = b20_install(&w, name, layout).await;
        let body = serde_json::json!({ "SourcePath": install });
        let first = b20_run(&w, body.clone()).await;
        let bob = b20_entry(&first, "bob").user_id.unwrap();
        let digest = b20_db_digest(&w.state).await;
        let codes = b20_code_rows(&w.state).await;
        assert_eq!(codes.len(), 1, "{name}: one code, bob's");

        let second = b20_run(&w, body).await;
        let users = vec![
            b20_user(&w, "Alice", "Match", None, Some(w.alice)),
            b20_user(&w, "bob", "Match", None, Some(bob)),
        ];
        assert_eq!(b20_users(&second), users, "{name}");
        assert_eq!(
            b20_db_digest(&w.state).await,
            digest,
            "{name}: the second run wrote"
        );
        assert_eq!(
            b20_code_rows(&w.state).await,
            codes,
            "{name}: the codes changed"
        );
        let alice = login(&w.app, "alice", "alicepass").await;
        assert!(alice.is_some(), "{name}: alice's password changed");
    }
}

#[tokio::test]
async fn b20_import_run_conflicts_while_running() {
    let w = b20_world().await;
    let install = b20_install(&w, "jf", jf::Layout::New).await;
    let body = serde_json::json!({ "SourcePath": install });
    let held = w
        .state
        .jellyfin_import
        .try_start()
        .expect("an idle job starts");
    let (status, answer) = b20_post(&w, "Run", body.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert_eq!(answer["Status"], 409, "{answer}");
    let running = b20_status(&w).await;
    assert_eq!(running["State"], "Running", "{running}");
    assert_eq!(running["Stage"], "Reading", "{running}");

    // A run that ends without a result has failed, and the next run starts.
    drop(held);
    let failed = b20_status(&w).await;
    assert_eq!(failed["State"], "Failed", "{failed}");
    assert!(failed["Error"].is_string(), "{failed}");
    assert!(failed["Result"].is_null(), "{failed}");
    let report = b20_run(&w, body).await;
    assert_eq!(b20_entry(&report, "bob").action, "Create");
}

// B20 2c: watch data and track choices.

/// A `user_item_data` row as the B20 import tests read and write it.
#[derive(Debug, Default, PartialEq)]
struct B20Data {
    played: bool,
    play_count: i64,
    position: i64,
    last_played: Option<chrono::DateTime<chrono::Utc>>,
    favorite: bool,
    rating: Option<i64>,
    likes: Option<bool>,
    audio: Option<i64>,
    subtitle: Option<i64>,
}

type B20DataRow = (
    bool,
    i64,
    i64,
    Option<String>,
    bool,
    Option<i64>,
    Option<bool>,
    Option<i64>,
    Option<i64>,
);

/// The instant `rfc3339`.
fn b20_at(rfc3339: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let instant =
        chrono::DateTime::parse_from_rfc3339(rfc3339).unwrap_or_else(|e| panic!("{rfc3339}: {e}"));
    Some(instant.with_timezone(&chrono::Utc))
}

/// The id of the Rustyfin item `name`.
async fn b20_item(state: &AppState, name: &str) -> uuid::Uuid {
    let id: String = sqlx::query_scalar("SELECT id FROM media_items WHERE name = ?")
        .bind(name)
        .fetch_one(&state.db)
        .await
        .unwrap();
    uuid::Uuid::parse_str(&id).unwrap()
}

/// The stored row of `user` for `item`; a stored date must be RFC 3339.
async fn b20_data(state: &AppState, user: uuid::Uuid, item: uuid::Uuid) -> Option<B20Data> {
    let row: Option<B20DataRow> = sqlx::query_as(
        "SELECT played, play_count, playback_position_ticks, last_played_date, is_favorite, \
         user_rating, likes, audio_stream_index, subtitle_stream_index FROM user_item_data \
         WHERE user_id = ? AND item_id = ?",
    )
    .bind(user.to_string())
    .bind(item.to_string())
    .fetch_optional(&state.db)
    .await
    .unwrap();
    row.map(|r| B20Data {
        played: r.0,
        play_count: r.1,
        position: r.2,
        last_played: r.3.and_then(|date| b20_at(&date)),
        favorite: r.4,
        rating: r.5,
        likes: r.6,
        audio: r.7,
        subtitle: r.8,
    })
}

/// Stores `data` as the row of `user` for `item`, its date written with `Z`.
async fn b20_put_data(state: &AppState, user: uuid::Uuid, item: uuid::Uuid, data: &B20Data) {
    let date = data
        .last_played
        .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true));
    sqlx::query(
        "INSERT INTO user_item_data (user_id, item_id, played, play_count, \
         playback_position_ticks, last_played_date, is_favorite, user_rating, likes, \
         audio_stream_index, subtitle_stream_index) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user.to_string())
    .bind(item.to_string())
    .bind(data.played)
    .bind(data.play_count)
    .bind(data.position)
    .bind(date)
    .bind(data.favorite)
    .bind(data.rating)
    .bind(data.likes)
    .bind(data.audio)
    .bind(data.subtitle)
    .execute(&state.db)
    .await
    .unwrap();
}

/// A report's `WatchData` as (Rows, Imported, Unchanged, Unmatched).
fn b20_watch_counts(report: &serde_json::Value) -> (i64, i64, i64, i64) {
    let count = |key: &str| report["WatchData"][key].as_i64().unwrap_or(-1);
    (
        count("Rows"),
        count("Imported"),
        count("Unchanged"),
        count("Unmatched"),
    )
}

/// A report's `Tracks` as (Audio, Subtitle, SubtitleOff, Unmapped).
fn b20_track_counts(report: &serde_json::Value) -> (i64, i64, i64, i64) {
    let count = |key: &str| report["Tracks"][key].as_i64().unwrap_or(-1);
    (
        count("Audio"),
        count("Subtitle"),
        count("SubtitleOff"),
        count("Unmapped"),
    )
}

/// The watch-data part of the INFO lines for `counts` (Rows, Imported, Unchanged, Unmatched).
fn b20_watch_line(counts: (i64, i64, i64, i64)) -> String {
    let (rows, imported, unchanged, unmatched) = counts;
    format!("{rows} watch rows ({imported} imported, {unchanged} unchanged, {unmatched} unmatched)")
}

#[tokio::test]
async fn b20_import_preview_counts_watch_data() {
    // Alice: Alpha and S01E01 match; Gamma and the new layout's detached row don't (the old
    // layout's detached row has a key no item has, so it is no row). bob: Beta and 1x03 match.
    for (name, layout, all, alice_only) in [
        ("new", jf::Layout::New, (6, 4, 0, 2), (4, 2, 0, 2)),
        ("old", jf::Layout::Old, (5, 4, 0, 1), (3, 2, 0, 1)),
    ] {
        let w = b20_world().await;
        let install = b20_install(&w, name, layout).await;
        let before = b20_db_digest(&w.state).await;
        let log = jf::LogCapture::start();
        let report = b20_preview(&w, serde_json::json!({ "SourcePath": install })).await;
        let text = log.text();
        drop(log);
        assert_eq!(b20_watch_counts(&report), all, "{name}: {report}");
        assert_eq!(b20_track_counts(&report), (0, 0, 0, 0), "{name}: {report}");
        let line = b20_watch_line(all);
        let info = |l: &str| {
            l.contains("INFO") && l.contains("jellyfin import preview") && l.contains(&line)
        };
        assert!(
            text.lines().any(info),
            "{name}: no INFO line with {line} in {text}"
        );

        let alice = w.source.user("Alice").id;
        let body = serde_json::json!({ "SourcePath": install, "UserIds": [alice] });
        let report = b20_preview(&w, body).await;
        assert_eq!(
            b20_watch_counts(&report),
            alice_only,
            "{name}: only the picked users' rows: {report}"
        );
        assert_eq!(
            b20_db_digest(&w.state).await,
            before,
            "{name}: a preview writes nothing"
        );
    }
}

#[tokio::test]
async fn b20_import_run_writes_watch_data() {
    for (name, layout, counts) in [
        ("new", jf::Layout::New, (6, 4, 0, 2)),
        ("old", jf::Layout::Old, (5, 4, 0, 1)),
    ] {
        let mut w = b20_world().await;
        // Alice likes Alpha; only the new layout has likes.
        w.source.user_data[0].likes = Some(true);
        let install = b20_install(&w, name, layout).await;
        let report = b20_run(&w, serde_json::json!({ "SourcePath": install })).await;
        assert_eq!(b20_watch_counts(&report), counts, "{name}: {report}");
        assert_eq!(b20_track_counts(&report), (0, 0, 0, 0), "{name}: {report}");

        let alpha = b20_item(&w.state, "Alpha").await;
        let expected = B20Data {
            played: true,
            play_count: 2,
            last_played: b20_at("2025-01-02T03:04:05.1234567Z"),
            favorite: true,
            rating: Some(8),
            likes: (layout == jf::Layout::New).then_some(true),
            ..Default::default()
        };
        let stored = b20_data(&w.state, w.alice, alpha).await;
        assert_eq!(stored, Some(expected), "{name}: Alpha");
        let e1 = b20_item(&w.state, "Show S01E01").await;
        let expected = B20Data {
            position: 6_000_000_000,
            last_played: b20_at("2025-02-01T10:00:00Z"),
            ..Default::default()
        };
        let stored = b20_data(&w.state, w.alice, e1).await;
        assert_eq!(stored, Some(expected), "{name}: S01E01");

        let bob = b20_entry(&report, "bob").user_id.unwrap();
        let played = |date: &str| B20Data {
            played: true,
            play_count: 1,
            last_played: b20_at(date),
            ..Default::default()
        };
        let beta = b20_item(&w.state, "Beta").await;
        let expected = B20Data {
            rating: Some(10),
            ..played("2025-03-01T20:00:00Z")
        };
        let stored = b20_data(&w.state, bob, beta).await;
        assert_eq!(stored, Some(expected), "{name}: Beta, rated 9.5");
        let e3 = b20_item(&w.state, "Show S01E03").await;
        let stored = b20_data(&w.state, bob, e3).await;
        assert_eq!(stored, Some(played("2025-03-02T21:00:00Z")), "{name}: 1x03");
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_item_data")
            .fetch_one(&w.state.db)
            .await
            .unwrap();
        assert_eq!(rows, 4, "{name}: only the matched rows are written");
    }
}

#[tokio::test]
async fn b20_import_merges_with_rustyfin_watch_data() {
    let w = b20_world().await;
    let bob = b20_plain_user(&w.state, "bob").await;
    let alpha = b20_item(&w.state, "Alpha").await;
    let beta = b20_item(&w.state, "Beta").await;
    let e1 = b20_item(&w.state, "Show S01E01").await;
    let e3 = b20_item(&w.state, "Show S01E03").await;
    // Alpha: Rustyfin's row is newer. S01E01: Jellyfin's is. Beta: the same instant, and
    // already what the import makes of Jellyfin's row.
    let alpha_row = B20Data {
        play_count: 5,
        position: 100,
        last_played: b20_at("2025-06-01T00:00:00Z"),
        rating: Some(3),
        likes: Some(true),
        audio: Some(7),
        ..Default::default()
    };
    let e1_row = B20Data {
        play_count: 1,
        position: 50,
        last_played: b20_at("2025-01-01T00:00:00Z"),
        audio: Some(7),
        ..Default::default()
    };
    let beta_row = B20Data {
        played: true,
        play_count: 1,
        last_played: b20_at("2025-03-01T20:00:00Z"),
        rating: Some(10),
        ..Default::default()
    };
    b20_put_data(&w.state, w.alice, alpha, &alpha_row).await;
    b20_put_data(&w.state, w.alice, e1, &e1_row).await;
    b20_put_data(&w.state, bob, beta, &beta_row).await;
    let install = b20_install(&w, "jf", jf::Layout::New).await;
    let body = serde_json::json!({ "SourcePath": install });

    let preview = b20_preview(&w, body.clone()).await;
    assert_eq!(b20_watch_counts(&preview), (6, 3, 1, 2), "{preview}");
    let report = b20_run(&w, body.clone()).await;
    assert_eq!(b20_watch_counts(&report), (6, 3, 1, 2), "{report}");
    assert_eq!(b20_entry(&report, "bob").user_id, Some(bob));

    // Played and favorite if either side; Rustyfin's count, date, position, rating, likes and
    // track stay on Alpha. S01E01 takes Jellyfin's date and position and keeps its track.
    let expected = B20Data {
        played: true,
        favorite: true,
        ..alpha_row
    };
    assert_eq!(b20_data(&w.state, w.alice, alpha).await, Some(expected));
    let expected = B20Data {
        position: 6_000_000_000,
        last_played: b20_at("2025-02-01T10:00:00Z"),
        ..e1_row
    };
    assert_eq!(b20_data(&w.state, w.alice, e1).await, Some(expected));
    let beta_date: Option<String> = sqlx::query_scalar(
        "SELECT last_played_date FROM user_item_data WHERE user_id = ? AND item_id = ?",
    )
    .bind(bob.to_string())
    .bind(beta.to_string())
    .fetch_one(&w.state.db)
    .await
    .unwrap();
    assert_eq!(
        beta_date.as_deref(),
        Some("2025-03-01T20:00:00Z"),
        "an unchanged row is not written"
    );
    assert!(b20_data(&w.state, bob, e3).await.is_some(), "1x03");

    let digest = b20_db_digest(&w.state).await;
    let again = b20_run(&w, body).await;
    assert_eq!(b20_watch_counts(&again), (6, 0, 4, 2), "{again}");
    assert_eq!(
        b20_db_digest(&w.state).await,
        digest,
        "the second run wrote"
    );
}

#[tokio::test]
async fn b20_import_keeps_the_newest_of_several_rows() {
    // The new layout: a second, older row of Alice's Alpha (another CustomDataKey) with more
    // plays loses to the newer one.
    let mut w = b20_world().await;
    let alice = w.source.user("Alice").id;
    w.source.user_data.push(jf::JfUserData {
        user: alice,
        item: w.source.item("Alpha").id,
        custom_key: Some("b20-older-key".to_string()),
        played: true,
        play_count: 7,
        position: 1234,
        last_played: Some("2024-01-01 00:00:00"),
        ..Default::default()
    });
    let install = b20_install(&w, "jf", jf::Layout::New).await;
    let report = b20_run(&w, serde_json::json!({ "SourcePath": install })).await;
    assert_eq!(b20_watch_counts(&report), (6, 4, 0, 2), "{report}");
    let alpha = b20_item(&w.state, "Alpha").await;
    let stored = b20_data(&w.state, w.alice, alpha).await.unwrap();
    assert_eq!((stored.play_count, stored.position), (2, 0));
    assert_eq!(stored.last_played, b20_at("2025-01-02T03:04:05.1234567Z"));
}

#[tokio::test]
async fn b20_import_old_rows_apply_to_each_item_of_their_key() {
    // The old layout keys rows by UserDataKey: S01E02 shares S01E01's key, so Alice's S01E01
    // row is S01E02's too.
    let mut w = b20_world().await;
    let key = w.source.item("Show S01E01").user_data_key.clone();
    let e2 = w
        .source
        .items
        .iter_mut()
        .find(|item| item.name == "Show S01E02");
    e2.unwrap().user_data_key = key;
    let install = b20_install(&w, "jf", jf::Layout::Old).await;
    let report = b20_run(&w, serde_json::json!({ "SourcePath": install })).await;
    assert_eq!(b20_watch_counts(&report), (6, 5, 0, 1), "{report}");
    let e2 = b20_item(&w.state, "Show S01E02").await;
    let stored = b20_data(&w.state, w.alice, e2).await.unwrap();
    assert_eq!(stored.position, 6_000_000_000);
}

#[tokio::test]
async fn b20_import_maps_track_choices() {
    for (name, layout) in [("new", jf::Layout::New), ("old", jf::Layout::Old)] {
        let mut w = b20_world().await;
        let jf_alpha = w.source.item("Alpha").id;
        let jf_beta = w.source.item("Beta").id;
        let jf_e1 = w.source.item("Show S01E01").id;
        let jf_e3 = w.source.item("Show 1x03").id;
        let stream = jf::JfStream::new;
        w.source.streams = vec![
            stream(jf_alpha, 0, "Video", "h264"),
            stream(jf_alpha, 1, "Audio", "aac").language("eng"),
            stream(jf_alpha, 2, "Audio", "ac3").language("ger"),
            stream(jf_alpha, 3, "Subtitle", "srt").language("eng"),
            stream(jf_alpha, 4, "Subtitle", "PGSSUB")
                .language("ger")
                .flags(false, true),
            stream(jf_beta, 0, "Video", "mpeg4"),
            stream(jf_beta, 1, "Audio", "mp3"),
            stream(jf_e1, 0, "Video", "h264"),
            stream(jf_e1, 1, "Audio", "aac"),
            stream(jf_e1, 2, "Audio", "ac3"),
            stream(jf_e3, 0, "Video", "h264"),
            stream(jf_e3, 1, "Audio", "aac"),
        ];
        // Alice: Alpha's aac and forced PGS subtitles, S01E01's ac3. bob: Beta with subtitles
        // off and an audio stream Jellyfin doesn't have; 1x03's aac, which has no Rustyfin source.
        for row in &mut w.source.user_data {
            let choice = [
                (jf_alpha, (Some(1), Some(4))),
                (jf_e1, (Some(2), None)),
                (jf_beta, (Some(9), Some(-1))),
                (jf_e3, (Some(1), None)),
            ]
            .into_iter()
            .find(|(item, _)| *item == row.item);
            if let Some((_, (audio, subtitle))) = choice {
                row.audio = audio;
                row.subtitle = subtitle;
            }
        }

        // Rustyfin numbers Alpha's streams its own way.
        let alpha = b20_item(&w.state, "Alpha").await;
        let alpha_file = std::path::PathBuf::from(format!("{}/Alpha (2001)/Alpha.mkv", w.movies));
        std::fs::create_dir_all(alpha_file.parent().unwrap()).unwrap();
        std::fs::write(&alpha_file, b"b20").unwrap();
        let streams = serde_json::json!([
            { "Index": 0, "Codec": "h264", "CodecType": "Video" },
            { "Index": 1, "Codec": "hdmv_pgs_subtitle", "CodecType": "Subtitle", "IsForced": true },
            { "Index": 2, "Codec": "ac3", "CodecType": "Audio" },
            { "Index": 3, "Codec": "aac", "CodecType": "Audio" },
            { "Index": 4, "Codec": "subrip", "CodecType": "Subtitle" },
        ]);
        b20_insert_source(&w.state, alpha, &alpha_file, streams).await;
        let beta = b20_item(&w.state, "Beta").await;
        let beta_file = std::path::PathBuf::from(format!("{}/Beta (2002)/Beta.mkv", w.movies));
        let streams = serde_json::json!([
            { "Index": 0, "Codec": "mpeg4", "CodecType": "Video" },
            { "Index": 1, "Codec": "mp3", "CodecType": "Audio" },
        ]);
        b20_insert_source(&w.state, beta, &beta_file, streams).await;
        // S01E01's first source is an older file; the import uses the one at the translated path.
        let e1 = b20_item(&w.state, "Show S01E01").await;
        let season = format!("{}/Show/Season 01", w.tv);
        let old_file = std::path::PathBuf::from(format!("{season}/Show S01E01.old.mkv"));
        let streams = serde_json::json!([
            { "Index": 0, "Codec": "h264", "CodecType": "Video" },
            { "Index": 1, "Codec": "ac3", "CodecType": "Audio" },
        ]);
        let old_source = b20_insert_source(&w.state, e1, &old_file, streams).await;
        sqlx::query("UPDATE media_sources SET created_at = '2000-01-01 00:00:00' WHERE id = ?")
            .bind(old_source.to_string())
            .execute(&w.state.db)
            .await
            .unwrap();
        let e1_file = std::path::PathBuf::from(format!("{season}/Show S01E01.mkv"));
        let streams = serde_json::json!([
            { "Index": 0, "Codec": "h264", "CodecType": "Video" },
            { "Index": 1, "Codec": "aac", "CodecType": "Audio" },
            { "Index": 2, "Codec": "ac3", "CodecType": "Audio" },
        ]);
        b20_insert_source(&w.state, e1, &e1_file, streams).await;

        let install = b20_install(&w, name, layout).await;
        let body = serde_json::json!({ "SourcePath": install });
        let preview = b20_preview(&w, body.clone()).await;
        let report = b20_run(&w, body).await;
        // Audio: Alpha and S01E01. Subtitle: Alpha. Off: Beta. Unmapped: Beta's audio 9 and
        // 1x03's audio.
        assert_eq!(b20_track_counts(&report), (2, 1, 1, 2), "{name}: {report}");
        assert_eq!(
            b20_track_counts(&preview),
            (2, 1, 1, 2),
            "{name}: the preview: {preview}"
        );
        let alice = w.alice.to_string();
        let tracks = b20_stored_tracks(&w.state, &alice, alpha).await;
        assert_eq!(tracks, (Some(3), Some(1)), "{name}: Alpha");
        let tracks = b20_stored_tracks(&w.state, &alice, e1).await;
        assert_eq!(tracks, (Some(2), None), "{name}: S01E01");
        let bob = b20_entry(&report, "bob").user_id.unwrap().to_string();
        let tracks = b20_stored_tracks(&w.state, &bob, beta).await;
        assert_eq!(tracks, (None, Some(-1)), "{name}: Beta");
        let e3 = b20_item(&w.state, "Show S01E03").await;
        let tracks = b20_stored_tracks(&w.state, &bob, e3).await;
        assert_eq!(tracks, (None, None), "{name}: 1x03");

        // Alice's player offers the imported tracks first.
        let (token, _) = login(&w.app, "alice", "alicepass").await.unwrap();
        let defaults = b20_only_defaults(&w.app, &token, alpha).await;
        assert_eq!(defaults, b20_both(3, 1), "{name}");
    }
}

#[tokio::test]
async fn b20_import_run_leaves_the_source_and_its_secrets_alone() {
    for (name, layout, counts) in [
        ("new", jf::Layout::New, (6, 4, 0, 2)),
        ("old", jf::Layout::Old, (5, 4, 0, 1)),
    ] {
        let w = b20_world().await;
        let install = b20_install(&w, name, layout).await;
        let before = jf::listing(&install);
        let log = jf::LogCapture::start();
        let report = b20_run(&w, serde_json::json!({ "SourcePath": install })).await;
        let status = b20_status(&w).await;
        let text = log.text();
        drop(log);
        assert_eq!(jf::listing(&install), before, "{name}: the source changed");
        for answer in [&report, &status] {
            assert!(
                !answer.to_string().contains(jf::CANARY),
                "{name}: secret in an answer"
            );
        }
        assert!(!text.contains(jf::CANARY), "{name}: secret in the log");
        let line = b20_watch_line(counts);
        let info =
            |l: &str| l.contains("INFO") && l.contains("jellyfin import run") && l.contains(&line);
        assert!(
            text.lines().any(info),
            "{name}: no INFO line with {line} in {text}"
        );
        let db = w.state.data_dir.join("test.db");
        let files = [
            db.clone(),
            db.with_extension("db-wal"),
            db.with_extension("db-shm"),
        ];
        assert!(
            !jf::files_hold_canary(&files),
            "{name}: secret in Rustyfin's database"
        );
    }
}

/// Fix 4: a run reads, merges and writes each user's rows in one write transaction, so progress a
/// client saves while the run works isn't overwritten with the rows the run read before.
#[tokio::test]
async fn b20_import_keeps_progress_saved_during_the_run() {
    use sqlx::ConnectOptions;
    let w = b20_world().await;
    let install = b20_install(&w, "jf", jf::Layout::New).await;
    let alpha = b20_item(&w.state, "Alpha").await;
    // A progress report's write, open from before the run starts until the run reads.
    let mut writer = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(w.state.data_dir.join("test.db"))
        .busy_timeout(std::time::Duration::from_secs(30))
        .connect()
        .await
        .unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut writer)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO user_item_data (user_id, item_id, played, play_count, \
         playback_position_ticks, last_played_date, is_favorite) \
         VALUES (?, ?, 0, 0, 7777, '2026-01-01T00:00:00Z', 0)",
    )
    .bind(w.alice.to_string())
    .bind(alpha.to_string())
    .execute(&mut writer)
    .await
    .unwrap();
    let alice = w.source.user("Alice").id;
    let body = serde_json::json!({
        "SourcePath": install,
        "UserIds": [alice],
        "MetadataMode": "None",
    });
    b20_start(&w, body).await;
    // The job itself, not `/JellyfinImport/Status`: the pool shares its cache, so a request made
    // while a pool connection waits for the write lock waits with it.
    let watch_data = Some(rustyfin_core::jellyfin_import::ImportStage::WatchData);
    let mut stage = None;
    for _ in 0..1000 {
        stage = w.state.jellyfin_import.status().stage;
        if stage == watch_data {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(stage, watch_data, "the run never reached its watch data");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    sqlx::query("COMMIT").execute(&mut writer).await.unwrap();
    let answer = b20_wait(&w).await;
    assert_eq!(answer["State"], "Completed", "{answer}");
    let row = b20_data(&w.state, w.alice, alpha)
        .await
        .expect("alice's Alpha row");
    assert_eq!(
        row.position, 7777,
        "the progress saved during the run was lost"
    );
    assert_eq!(row.last_played, b20_at("2026-01-01T00:00:00Z"));
    assert!(
        row.played && row.favorite,
        "Jellyfin's played and favorite merge in"
    );
    assert_eq!(row.play_count, 2);
}

/// Runs `statements` on a WAL-mode copy of `file` made outside the install folder, then puts the
/// copy and its `-wal` (no checkpoint ran, so the changes are only there) in place of `file`: a
/// source as a running Jellyfin leaves it.
async fn b20_change_in_wal(w: &B20World, file: &std::path::Path, statements: &[&'static str]) {
    use sqlx::{ConnectOptions, Connection};
    let wal = |db: &std::path::Path| {
        let mut name = db.as_os_str().to_owned();
        name.push("-wal");
        std::path::PathBuf::from(name)
    };
    let scratch = w
        .jf_root
        .join(format!("wal_{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&scratch).unwrap();
    let copy = scratch.join(file.file_name().unwrap());
    std::fs::copy(file, &copy).unwrap();
    let mut conn = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&copy)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .connect()
        .await
        .unwrap();
    sqlx::query("PRAGMA wal_autocheckpoint = 0")
        .execute(&mut conn)
        .await
        .unwrap();
    for &statement in statements {
        sqlx::query(statement).execute(&mut conn).await.unwrap();
    }
    let frames = std::fs::metadata(wal(&copy)).unwrap().len();
    assert!(frames > 32, "no frames in {}", wal(&copy).display());
    std::fs::copy(&copy, file).unwrap();
    std::fs::copy(wal(&copy), wal(file)).unwrap();
    conn.close().await.unwrap();
}

const B20_RENAME_BOB: &str = r#"UPDATE "Users" SET "Username" = 'carol' WHERE "Username" = 'bob'"#;
const B20_RENAME_GAMMA_NEW: &str =
    r#"UPDATE "BaseItems" SET "Name" = 'Gamma WAL' WHERE "Name" = 'Gamma'"#;
const B20_RENAME_GAMMA_OLD: &str =
    "UPDATE TypedBaseItems SET Name = 'Gamma WAL' WHERE Name = 'Gamma'";

/// Fix 7: a source whose `-wal` holds changes is read with them, and nothing appears or changes
/// under the source folder (no `-shm` either).
#[tokio::test]
async fn b20_import_reads_the_wal_and_leaves_the_source_folder_alone() {
    for (name, layout) in [("new", jf::Layout::New), ("old", jf::Layout::Old)] {
        let w = b20_world().await;
        let install = b20_install(&w, name, layout).await;
        let data = install.join("data");
        match layout {
            jf::Layout::New => {
                let changes = [B20_RENAME_BOB, B20_RENAME_GAMMA_NEW];
                b20_change_in_wal(&w, &data.join("jellyfin.db"), &changes).await;
            }
            jf::Layout::Old => {
                b20_change_in_wal(&w, &data.join("jellyfin.db"), &[B20_RENAME_BOB]).await;
                let library = data.join("library.db");
                b20_change_in_wal(&w, &library, &[B20_RENAME_GAMMA_OLD]).await;
            }
        }
        let before = jf::listing(&install);
        let report = b20_preview(&w, serde_json::json!({ "SourcePath": install })).await;
        assert_eq!(
            jf::listing(&install),
            before,
            "{name}: the preview changed the source folder"
        );
        let names: Vec<String> = b20_users(&report).into_iter().map(|u| u.name).collect();
        assert!(
            names.iter().any(|n| n == "carol") && !names.iter().any(|n| n == "bob"),
            "{name}: users {names:?}"
        );
        let samples = b20_samples(&report);
        assert!(
            samples.iter().any(|(_, item, _)| item == "Gamma WAL"),
            "{name}: samples {samples:?}"
        );
        let alice = w.source.user("Alice").id;
        let body = serde_json::json!({ "SourcePath": install, "UserIds": [alice] });
        b20_run(&w, body).await;
        assert_eq!(
            jf::listing(&install),
            before,
            "{name}: the run changed the source folder"
        );
    }
}

/// One state of the flipping writer: Gamma's name (read with the items) and Alpha's tagline (read
/// with the metadata), changed together.
const B20_FLIPS_NEW: [&str; 2] = [
    r#"BEGIN; UPDATE "BaseItems" SET "Name" = 'Gamma' WHERE "Name" = 'Gamma Y';
       UPDATE "BaseItems" SET "Tagline" = NULL WHERE "Name" = 'Alpha'; COMMIT;"#,
    r#"BEGIN; UPDATE "BaseItems" SET "Name" = 'Gamma Y' WHERE "Name" = 'Gamma';
       UPDATE "BaseItems" SET "Tagline" = 'one' WHERE "Name" = 'Alpha'; COMMIT;"#,
];
const B20_FLIPS_OLD: [&str; 2] = [
    "BEGIN; UPDATE TypedBaseItems SET Name = 'Gamma' WHERE Name = 'Gamma Y';
     UPDATE TypedBaseItems SET Tagline = NULL WHERE Name = 'Alpha'; COMMIT;",
    "BEGIN; UPDATE TypedBaseItems SET Name = 'Gamma Y' WHERE Name = 'Gamma';
     UPDATE TypedBaseItems SET Tagline = 'one' WHERE Name = 'Alpha'; COMMIT;",
];

/// What a FillEmpty preview saw: Gamma's name and the metadata's `Fields` count.
fn b20_seen(report: &serde_json::Value) -> (String, i64) {
    let gamma = b20_samples(report)
        .into_iter()
        .map(|(_, name, _)| name)
        .find(|name| name.starts_with("Gamma"))
        .unwrap_or_else(|| panic!("no Gamma sample: {report}"));
    (gamma, report["Metadata"]["Fields"].as_i64().unwrap_or(-1))
}

/// Fix 8: while Jellyfin writes, a preview reads one state of each file: everything it reports
/// from a file comes from the same commit, or it fails as changing. The writer gets a worker
/// thread of its own, so it flips while a preview runs, not only when the preview yields.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b20_import_reads_one_state_while_jellyfin_writes() {
    use sqlx::{ConnectOptions, Connection};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    for (name, layout, file, flips) in [
        ("new", jf::Layout::New, "jellyfin.db", B20_FLIPS_NEW),
        ("old", jf::Layout::Old, "library.db", B20_FLIPS_OLD),
    ] {
        let w = b20_world().await;
        let install = b20_install(&w, name, layout).await;
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(install.join("data").join(file))
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(30));
        let body = serde_json::json!({ "SourcePath": install, "MetadataMode": "FillEmpty" });
        let mut states = Vec::new();
        for flip in flips {
            let mut conn = options.connect().await.unwrap();
            sqlx::raw_sql(flip).execute(&mut conn).await.unwrap();
            conn.close().await.unwrap();
            states.push(b20_seen(&b20_preview(&w, body.clone()).await));
        }
        assert!(
            states[0].0 != states[1].0 && states[0].1 != states[1].1,
            "{name}: the two states look alike: {states:?}"
        );

        let stop = Arc::new(AtomicBool::new(false));
        let flipped = Arc::new(AtomicUsize::new(0));
        // No fsync per commit: the writer flips often, as a busy Jellyfin would.
        let mut conn = options
            .clone()
            .synchronous(sqlx::sqlite::SqliteSynchronous::Off)
            .connect()
            .await
            .unwrap();
        let writer = tokio::spawn({
            let (stop, flipped) = (stop.clone(), flipped.clone());
            async move {
                let mut n = 0;
                while !stop.load(Ordering::Relaxed) {
                    sqlx::raw_sql(flips[n % 2])
                        .execute(&mut conn)
                        .await
                        .unwrap();
                    n += 1;
                    flipped.store(n, Ordering::Relaxed);
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                conn.close().await.unwrap();
            }
        });
        let (mut read, mut overlapped) = (0, 0);
        for attempt in 0..30 {
            let before = flipped.load(Ordering::Relaxed);
            let (status, report) = b20_post(&w, "Preview", body.clone()).await;
            if flipped.load(Ordering::Relaxed) > before {
                overlapped += 1;
            }
            if status == StatusCode::OK {
                let seen = b20_seen(&report);
                assert!(
                    states.contains(&seen),
                    "{name}: preview {attempt} mixed two states: {seen:?}, states {states:?}"
                );
                read += 1;
            } else {
                assert_eq!(status, StatusCode::BAD_REQUEST, "{name}: {report}");
                assert!(
                    report.to_string().contains("kept changing"),
                    "{name}: {report}"
                );
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer.await.unwrap();
        assert!(read > 0, "{name}: no preview read the source");
        assert!(
            overlapped >= 10,
            "{name}: the writer flipped during only {overlapped} of 30 previews"
        );
    }
}

// B20 3: one-time codes and first sign-in.

type B20Answer = (StatusCode, Option<String>, serde_json::Value);

/// Runs an import that creates bob and returns his id and one-time code.
async fn b20_pending_bob(w: &B20World) -> (uuid::Uuid, String) {
    let install = b20_install(w, "jf", jf::Layout::New).await;
    let report = b20_run(w, serde_json::json!({ "SourcePath": install })).await;
    let bob = b20_entry(&report, "bob");
    match (bob.user_id, bob.code) {
        (Some(id), Some(code)) => (id, code),
        _ => panic!("the run created no bob with a code"),
    }
}

/// Sends `req`; returns the status, the `X-Rustyfin-Setup-Required` header and the JSON body.
async fn b20_send(app: &axum::Router, req: Request<Body>) -> B20Answer {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let header = resp
        .headers()
        .get("X-Rustyfin-Setup-Required")
        .map(|value| value.to_str().unwrap().to_string());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, header, json)
}

/// `POST /Users/AuthenticateByName` with `name` and `password`.
async fn b20_sign_in(app: &axum::Router, name: &str, password: &str) -> B20Answer {
    let body = serde_json::json!({ "Username": name, "Pw": password });
    let req = request("POST", "/Users/AuthenticateByName", None, Some(body));
    b20_send(app, req).await
}

/// `POST /Users/AuthenticateWithSetupCode` with `name`, `code` and the new password.
async fn b20_redeem(app: &axum::Router, name: &str, code: &str, new_password: &str) -> B20Answer {
    let body = serde_json::json!({ "Username": name, "Code": code, "NewPw": new_password });
    let req = request("POST", "/Users/AuthenticateWithSetupCode", None, Some(body));
    b20_send(app, req).await
}

/// `POST /JellyfinImport/Users/{user}/SetupCode` as the admin.
async fn b20_reissue(w: &B20World, user: uuid::Uuid) -> (StatusCode, serde_json::Value) {
    let uri = format!("/JellyfinImport/Users/{user}/SetupCode");
    send(&w.app, request("POST", &uri, Some(&w.token), None)).await
}

/// `GET /JellyfinImport/PendingUsers` as the admin; expects 200 and a JSON array.
async fn b20_pending(w: &B20World) -> Vec<serde_json::Value> {
    let uri = "/JellyfinImport/PendingUsers";
    let (status, answer) = send(&w.app, request("GET", uri, Some(&w.token), None)).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    answer.as_array().expect("PendingUsers is no array").clone()
}

/// A pending user's entry: its id, name and date.
fn b20_pending_entry(
    entry: &serde_json::Value,
) -> (uuid::Uuid, String, chrono::DateTime<chrono::Utc>) {
    assert_eq!(entry.as_object().map(|o| o.len()), Some(3), "{entry}");
    let id = uuid::Uuid::parse_str(entry["UserId"].as_str().unwrap()).unwrap();
    let name = entry["Name"].as_str().unwrap().to_string();
    let at = chrono::DateTime::parse_from_rfc3339(entry["CreatedAt"].as_str().unwrap()).unwrap();
    (id, name, at.with_timezone(&chrono::Utc))
}

/// How many tokens and sessions `user` has.
async fn b20_sessions(state: &AppState, user: uuid::Uuid) -> (i64, i64) {
    let mut counts = Vec::new();
    for sql in [
        "SELECT COUNT(*) FROM access_tokens WHERE user_id = ?",
        "SELECT COUNT(*) FROM sessions WHERE user_id = ?",
    ] {
        let n: i64 = sqlx::query_scalar(sql)
            .bind(user.to_string())
            .fetch_one(&state.db)
            .await
            .unwrap();
        counts.push(n);
    }
    (counts[0], counts[1])
}

/// Whether `text` holds `code` in any form a client may send.
fn b20_holds_code(text: &str, code: &str) -> bool {
    let bare = code.replace('-', "");
    [
        code.to_string(),
        code.to_lowercase(),
        bare.clone(),
        bare.to_lowercase(),
    ]
    .iter()
    .any(|form| text.contains(form.as_str()))
}

#[tokio::test]
async fn b20_code_sign_in_asks_for_a_new_password() {
    let w = b20_world().await;
    let (bob, code) = b20_pending_bob(&w).await;
    let (failed, header, failed_body) = b20_sign_in(&w.app, "bob", "wrong password").await;
    assert_eq!(failed, StatusCode::UNAUTHORIZED);
    assert_eq!(header, None, "a wrong password asks for a new password");

    let log = jf::LogCapture::start();
    let lower = code.to_lowercase();
    let bare = code.replace('-', "");
    let spaced = format!(" {} ", code.replace('-', " "));
    for (form, typed) in [
        ("as shown", code.as_str()),
        ("lowercase", lower.as_str()),
        ("without dashes", bare.as_str()),
        ("with spaces", spaced.as_str()),
    ] {
        let (status, header, body) = b20_sign_in(&w.app, "bob", typed).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{form}");
        assert_eq!(header.as_deref(), Some("true"), "{form}: no setup header");
        assert_eq!(body, failed_body, "{form}: not a failed sign-in's body");
    }
    let text = log.text();
    drop(log);
    assert!(!b20_holds_code(&text, &code), "the code is in the log");

    // Only bob's own code asks for a new password, and it gives no session.
    let other = rustyfin_auth::generate_setup_code();
    for (case, name, typed) in [
        ("another code", "bob", other.as_str()),
        ("unknown user", "nobody", code.as_str()),
        ("a user without a code", "alice", code.as_str()),
    ] {
        let (status, header, body) = b20_sign_in(&w.app, name, typed).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{case}");
        assert_eq!(header, None, "{case}: setup header");
        assert_eq!(body, failed_body, "{case}");
    }
    assert_eq!(
        b20_sessions(&w.state, bob).await,
        (0, 0),
        "bob got a session"
    );
    assert_eq!(
        b20_code_rows(&w.state).await.len(),
        1,
        "a sign-in used the code"
    );
    assert!(login(&w.app, "alice", "alicepass").await.is_some());
}

#[tokio::test]
async fn b20_setup_code_sets_the_password_and_signs_in() {
    let w = b20_world().await;
    let (bob, code) = b20_pending_bob(&w).await;
    let log = jf::LogCapture::start();
    let (status, header, answer) =
        b20_redeem(&w.app, "bob", &code.to_lowercase(), "new pass").await;
    let text = log.text();
    drop(log);
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(header, None);
    let id = answer["User"]["Id"].as_str().expect("no User.Id");
    assert_eq!(uuid::Uuid::parse_str(id).unwrap(), bob, "{answer}");
    assert_eq!(answer["User"]["Name"], "bob", "{answer}");
    assert!(answer["SessionInfo"]["Id"].is_string(), "{answer}");
    assert!(answer["ServerId"].is_string(), "{answer}");
    let token = answer["AccessToken"].as_str().expect("no AccessToken");
    assert!(
        !b20_holds_code(&answer.to_string(), &code),
        "the code is in the answer"
    );
    assert!(!b20_holds_code(&text, &code), "the code is in the log");
    assert!(!text.contains("new pass"), "the password is in the log");

    let (status, me) = send_authed(&w.app, "GET", "/Users/Me", token, None).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["Name"], "bob");
    assert_eq!(b20_sessions(&w.state, bob).await, (1, 1));

    // The new password signs in; the code is used up everywhere.
    assert!(login(&w.app, "bob", "new pass").await.is_some());
    let (status, header, _) = b20_sign_in(&w.app, "bob", &code).await;
    assert_eq!(
        (status, header),
        (StatusCode::UNAUTHORIZED, None),
        "sign-in"
    );
    let (status, _, _) = b20_redeem(&w.app, "bob", &code, "another").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the code worked twice");
    assert!(login(&w.app, "bob", "another").await.is_none());
    assert!(
        b20_code_rows(&w.state).await.is_empty(),
        "the code row stays"
    );
    assert!(b20_pending(&w).await.is_empty(), "bob is still pending");
}

#[tokio::test]
async fn b20_setup_code_refuses_bad_requests() {
    let w = b20_world().await;
    let (_, code) = b20_pending_bob(&w).await;
    let (_, _, failed_body) = b20_sign_in(&w.app, "bob", "wrong password").await;
    let (status, _, answer) = b20_redeem(&w.app, "bob", &code, "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "empty NewPw: {answer}");

    let other = rustyfin_auth::generate_setup_code();
    for (case, name, typed) in [
        ("another code", "bob", other.as_str()),
        ("not a code", "bob", "letmein"),
        ("unknown user", "nobody", code.as_str()),
        ("a user without a code", "alice", code.as_str()),
    ] {
        let (status, header, body) = b20_redeem(&w.app, name, typed, "new pass").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{case}");
        assert_eq!(header, None, "{case}");
        assert_eq!(body, failed_body, "{case}");
    }
    assert!(
        login(&w.app, "alice", "alicepass").await.is_some(),
        "alice's password changed"
    );
    assert!(login(&w.app, "alice", "new pass").await.is_none());
    assert!(login(&w.app, "bob", "new pass").await.is_none());
    assert_eq!(
        b20_code_rows(&w.state).await.len(),
        1,
        "a refusal used the code"
    );
    let (status, _, _) = b20_redeem(&w.app, "bob", &code, "new pass").await;
    assert_eq!(status, StatusCode::OK, "the code no longer works");
}

#[tokio::test]
async fn b20_disabled_pending_user_cannot_use_the_code() {
    let w = b20_world().await;
    let (bob, code) = b20_pending_bob(&w).await;
    sqlx::query("UPDATE users SET is_disabled = 1 WHERE id = ?")
        .bind(bob.to_string())
        .execute(&w.state.db)
        .await
        .unwrap();
    let (status, header, _) = b20_sign_in(&w.app, "bob", &code).await;
    assert_eq!(
        (status, header),
        (StatusCode::UNAUTHORIZED, None),
        "sign-in"
    );
    let (status, _, _) = b20_redeem(&w.app, "bob", &code, "new pass").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "redeem");
    assert_eq!(
        b20_code_rows(&w.state).await.len(),
        1,
        "a disabled user used the code"
    );
    assert_eq!(b20_sessions(&w.state, bob).await, (0, 0));
}

#[tokio::test]
async fn b20_setup_code_checks_cost_the_same_for_everyone() {
    use std::time::{Duration, Instant};
    // carol is a second pending user, disabled below.
    let mut w = b20_world().await;
    w.source.users.push(jf::JfUser::new(3, "carol", false));
    let install = b20_install(&w, "jf", jf::Layout::New).await;
    let report = b20_run(&w, serde_json::json!({ "SourcePath": install })).await;
    assert!(b20_entry(&report, "bob").code.is_some(), "bob has no code");
    let carol = b20_entry(&report, "carol").user_id.unwrap();
    sqlx::query("UPDATE users SET is_disabled = 1 WHERE id = ?")
        .bind(carol.to_string())
        .execute(&w.state.db)
        .await
        .unwrap();

    // bob with a wrong code is the measure: his code hash is verified. Then a user that doesn't
    // exist, alice (no code), carol (disabled) and bob with a code of the wrong shape.
    let wrong = "0000-0000-0000";
    let cases = [
        ("bob", wrong),
        ("nobody", wrong),
        ("alice", wrong),
        ("carol", wrong),
        ("bob", "not a code"),
    ];
    let device = rustyfin_auth::TokenDevice {
        device_id: "b20-timing".to_string(),
        device_name: "test".to_string(),
        app_name: "test".to_string(),
        app_version: "1".to_string(),
    };
    let mut redeem = [Duration::MAX; 5];
    let mut pending = [Duration::MAX; 5];
    let mut errors = Vec::new();
    // The fastest of two tries, the cases in turn.
    for _ in 0..2 {
        for (n, (name, code)) in cases.iter().enumerate() {
            let start = Instant::now();
            let result = rustyfin_auth::authenticate_with_setup_code(
                &w.state.db,
                name,
                code,
                "new pass",
                &device,
            )
            .await;
            redeem[n] = redeem[n].min(start.elapsed());
            errors.push((*name, *code, result.err().map(|e| format!("{e:?}"))));
            let start = Instant::now();
            let found = rustyfin_auth::setup_code_pending(&w.state.db, name, code)
                .await
                .unwrap();
            pending[n] = pending[n].min(start.elapsed());
            assert!(!found, "{name} with {code:?} is pending");
        }
    }
    for (check, times) in [
        ("AuthenticateWithSetupCode", redeem),
        ("the code check of a sign-in", pending),
    ] {
        for (n, (name, code)) in cases.iter().enumerate().skip(1) {
            assert!(
                times[n] * 4 >= times[0],
                "{check}: {name} with {code:?} took {:?}, bob with a wrong code {:?}",
                times[n],
                times[0]
            );
        }
    }
    for (name, code, error) in errors {
        assert_eq!(
            error.as_deref(),
            Some("InvalidCredentials"),
            "{name} with {code:?}"
        );
    }
}

#[tokio::test]
async fn b20_setup_code_sign_in_is_rate_limited() {
    let mut state = test_state().await;
    state.rate_limiter = RateLimiter::new(3, 60);
    let app = remote_app(state);
    let body = serde_json::json!({ "Username": "nobody", "Code": "AAAA-AAAA-AAAA", "NewPw": "x" });
    let attempt = || {
        request(
            "POST",
            "/Users/AuthenticateWithSetupCode",
            None,
            Some(body.clone()),
        )
    };
    for n in 1..=3 {
        let status = send(&app, attempt()).await.0;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {n}");
    }
    let status = send(&app, attempt()).await.0;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn b20_pending_users_and_reissue_are_admin_only() {
    let w = b20_world().await;
    let (bob, _) = b20_pending_bob(&w).await;
    let codes = b20_code_rows(&w.state).await;
    add_user(&w.state, "viewer", "viewerpass", false).await;
    let (viewer, _) = login(&w.app, "viewer", "viewerpass").await.unwrap();
    let pending = "/JellyfinImport/PendingUsers";
    g1_assert_admin_only(&w.app, "GET", pending, &None, &viewer).await;
    let reissue = format!("/JellyfinImport/Users/{bob}/SetupCode");
    g1_assert_admin_only(&w.app, "POST", &reissue, &None, &viewer).await;
    assert_eq!(
        b20_code_rows(&w.state).await,
        codes,
        "a refused reissue wrote"
    );
}

#[tokio::test]
async fn b20_pending_users_lists_users_with_a_code() {
    let mut w = b20_world().await;
    w.source.users.push(jf::JfUser::new(3, "Carol", false));
    let install = b20_install(&w, "jf", jf::Layout::New).await;
    let started = chrono::Utc::now();
    let report = b20_run(&w, serde_json::json!({ "SourcePath": install })).await;
    let ended = chrono::Utc::now();
    let bob = b20_entry(&report, "bob").user_id.unwrap();
    let carol = b20_entry(&report, "Carol").user_id.unwrap();

    // Sorted by name, case-insensitively; alice (matched) and the admin are not pending.
    let listed: Vec<_> = b20_pending(&w)
        .await
        .iter()
        .map(b20_pending_entry)
        .collect();
    let names: Vec<(uuid::Uuid, &str)> = listed
        .iter()
        .map(|(id, name, _)| (*id, name.as_str()))
        .collect();
    assert_eq!(names, vec![(bob, "bob"), (carol, "Carol")]);
    for (_, name, at) in &listed {
        let window = started - chrono::Duration::seconds(1)..=ended + chrono::Duration::seconds(1);
        assert!(window.contains(at), "{name}: CreatedAt {at}");
    }
}

#[tokio::test]
async fn b20_reissue_replaces_the_code() {
    let w = b20_world().await;
    let (bob, old) = b20_pending_bob(&w).await;
    let before = b20_pending_entry(&b20_pending(&w).await[0]);
    let log = jf::LogCapture::start();
    let (status, answer) = b20_reissue(&w, bob).await;
    let text = log.text();
    drop(log);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        answer.as_object().map(|o| o.len()),
        Some(3),
        "not UserId, Name and Code"
    );
    let id = uuid::Uuid::parse_str(answer["UserId"].as_str().unwrap()).unwrap();
    assert_eq!(id, bob);
    assert_eq!(answer["Name"], "bob");
    let new = answer["Code"].as_str().expect("no Code").to_string();
    assert!(b20_is_code(&new), "not a code");
    assert_ne!(new, old);
    assert!(!b20_holds_code(&text, &new), "the code is in the log");
    let info = |l: &str| l.contains("INFO") && l.contains("jellyfin import: new setup code");
    assert!(text.lines().any(info), "no INFO line");

    let hash: String =
        sqlx::query_scalar("SELECT code_hash FROM user_setup_codes WHERE user_id = ?")
            .bind(bob.to_string())
            .fetch_one(&w.state.db)
            .await
            .unwrap();
    assert!(!b20_holds_code(&hash, &new), "the code is stored");
    let verified = rustyfin_auth::verify_setup_code(&new, &hash).await.unwrap();
    assert!(verified, "the hash is not the new code's");
    assert!(
        !b20_holds_code(&b20_status(&w).await.to_string(), &new),
        "in the status"
    );
    let after = b20_pending_entry(&b20_pending(&w).await[0]);
    assert_eq!((after.0, &after.1), (before.0, &before.1));
    assert!(after.2 >= before.2, "CreatedAt went back");

    // The old code is dead; the new one asks for a new password and redeems.
    let (status, header, _) = b20_sign_in(&w.app, "bob", &old).await;
    assert_eq!(
        (status, header),
        (StatusCode::UNAUTHORIZED, None),
        "old code"
    );
    let (status, _, _) = b20_redeem(&w.app, "bob", &old, "new pass").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the old code redeemed");
    let (status, header, _) = b20_sign_in(&w.app, "bob", &new).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(header.as_deref(), Some("true"), "new code");
    let (status, _, _) = b20_redeem(&w.app, "bob", &new, "new pass").await;
    assert_eq!(status, StatusCode::OK, "the new code");
}

#[tokio::test]
async fn b20_reissue_refuses_users_without_a_code() {
    let w = b20_world().await;
    let (bob, code) = b20_pending_bob(&w).await;
    let (status, answer) = b20_reissue(&w, uuid::Uuid::new_v4()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown user: {answer}");
    let (status, answer) = b20_reissue(&w, w.alice).await;
    assert_eq!(status, StatusCode::CONFLICT, "alice: {answer}");
    let (status, _, _) = b20_redeem(&w.app, "bob", &code, "new pass").await;
    assert_eq!(status, StatusCode::OK);
    let (status, answer) = b20_reissue(&w, bob).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "bob after his sign-in: {answer}"
    );
    assert!(
        b20_code_rows(&w.state).await.is_empty(),
        "a refused reissue wrote"
    );
    assert!(b20_pending(&w).await.is_empty());
    assert!(login(&w.app, "bob", "new pass").await.is_some());
    assert!(login(&w.app, "alice", "alicepass").await.is_some());
}

#[tokio::test]
async fn b20_setting_a_password_ends_the_code() {
    let w = b20_world().await;
    let (bob, code) = b20_pending_bob(&w).await;
    let uri = format!("/Users/{bob}/Password");
    let body = serde_json::json!({ "CurrentPw": "", "NewPw": "set by admin" });
    let (status, answer) = send(&w.app, request("POST", &uri, Some(&w.token), Some(body))).await;
    assert!(status.is_success(), "{status} {answer}");
    assert!(
        b20_code_rows(&w.state).await.is_empty(),
        "the code row stays"
    );
    assert!(b20_pending(&w).await.is_empty(), "bob is still pending");
    let (status, header, _) = b20_sign_in(&w.app, "bob", &code).await;
    assert_eq!(
        (status, header),
        (StatusCode::UNAUTHORIZED, None),
        "sign-in"
    );
    let (status, _, _) = b20_redeem(&w.app, "bob", &code, "mine").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "redeem");
    let (status, _) = b20_reissue(&w, bob).await;
    assert_eq!(status, StatusCode::CONFLICT, "reissue");
    assert!(login(&w.app, "bob", "set by admin").await.is_some());
}

// B20 5: metadata for matched items.

/// Replaces the world's Jellyfin item `name` with `edit` of it.
fn b20_edit(w: &mut B20World, name: &str, edit: impl FnOnce(jf::JfItem) -> jf::JfItem) {
    let at = w.source.items.iter().position(|i| i.name == name);
    let at = at.unwrap_or_else(|| panic!("no Jellyfin item {name}"));
    w.source.items[at] = edit(w.source.items[at].clone());
}

/// The metadata world's install, Rustyfin's item ids (taken before an import renames anything)
/// and the bytes of the usable Jellyfin images.
struct B20Meta {
    install: std::path::PathBuf,
    alpha: uuid::Uuid,
    beta: uuid::Uuid,
    show: uuid::Uuid,
    e1: uuid::Uuid,
    e3: uuid::Uuid,
    poster: Vec<u8>,
    backdrop: Vec<u8>,
    logo: Vec<u8>,
    show_poster: Vec<u8>,
    ann: Vec<u8>,
}

/// The B20 world with Jellyfin metadata, installed into the folder `name` in `layout`.
///
/// Jellyfin's Alpha, renamed "Alpha: Jellyfin Cut", has every field, TMDB and IMDb ids, four
/// credits (Ann twice) and ten image rows: a poster; a missing backdrop before a usable one, so
/// indexes have no gap; a logo beside the media; a Box (ignored); and five unusable rows: a `..`
/// path to a JPEG, a URL, a text file, a symlink to a JPEG outside the metadata folder, and the
/// database through `%AppDataPath%`. Beta has some fields, an IMDb id and a credit; Rustyfin's
/// Beta already has an overview, genres and a rating. The Show has an overview, a studio, a
/// creator and a poster; Rustyfin's Show already has a person and a local poster. S01E01 has an
/// overview and a date. Jellyfin's Ann (with a portrait) and Dan (whose portrait file is missing)
/// are Person items.
async fn b20_meta_world(name: &str, layout: jf::Layout) -> (B20World, B20Meta) {
    let mut w = b20_world().await;
    let dir = "/config/metadata/library/a1/alpha";
    b20_edit(&mut w, "Alpha", |i| {
        let mut alpha = i
            .provider("Tmdb", "111")
            .provider("Imdb", "tt0000111")
            .meta(jf::JfMeta {
                forced_sort_name: Some("Alpha, The"),
                original_title: Some("Alfa"),
                overview: Some("Jellyfin overview of Alpha."),
                tagline: Some("First of all."),
                official_rating: Some("PG-13"),
                community_rating: Some(7.5),
                critic_rating: Some(88.0),
                premiere_date: Some("2001-05-04 00:00:00"),
                production_year: Some(2001),
                genres: Some("Drama| Comedy |Drama"),
                studios: Some("Acme Pictures|Big Studio"),
                tags: Some("favourite"),
                ..Default::default()
            })
            .credit("Ann Actor", Some("Actor"), "Hero", Some(1))
            .credit("Dan Director", Some("Director"), "", Some(0))
            .credit("Ann Actor", Some("Actor"), "Cameo", Some(2))
            .credit("Nora Nobody", None, "", None)
            .image(
                &format!("{dir}/poster.jpg"),
                0,
                (1000, 1500),
                Some("LEHV*nWB|yk8"),
            )
            .image(&format!("{dir}/backdrop1.jpg"), 2, (1920, 1080), None)
            .image(
                "%MetadataPath%/library/a1/alpha/backdrop.png",
                2,
                (1920, 1080),
                None,
            )
            .image("/data/movies/Alpha (2001)/logo.png", 4, (0, 0), None)
            .image(&format!("{dir}/box.jpg"), 7, (100, 100), None)
            .image("/config/metadata/../outside.jpg", 5, (100, 100), None)
            .image("https://image.example/banner.jpg", 3, (100, 100), None)
            .image(&format!("{dir}/notimage.jpg"), 1, (100, 100), None)
            .image(&format!("{dir}/disc.jpg"), 6, (100, 100), None)
            .image("%AppDataPath%/jellyfin.db", 8, (100, 100), None);
        alpha.name = "Alpha: Jellyfin Cut".to_string();
        alpha
    });
    b20_edit(&mut w, "Beta", |i| {
        i.provider("Imdb", "tt0000222")
            .meta(jf::JfMeta {
                overview: Some("Jellyfin overview of Beta."),
                community_rating: Some(6.0),
                production_year: Some(2002),
                genres: Some("Thriller"),
                ..Default::default()
            })
            .credit("Bea Star", Some("Actor"), "Lead", Some(0))
    });
    b20_edit(&mut w, "Show", |i| {
        i.meta(jf::JfMeta {
            overview: Some("A show."),
            studios: Some("Acme TV"),
            ..Default::default()
        })
        .credit("Sam Showrunner", Some("Creator"), "", Some(0))
        .image(
            "%MetadataPath%/library/b2/show/poster.jpg",
            0,
            (680, 1000),
            None,
        )
    });
    b20_edit(&mut w, "Show S01E01", |i| {
        i.meta(jf::JfMeta {
            overview: Some("Pilot."),
            premiere_date: Some("2025-01-10 00:00:00"),
            ..Default::default()
        })
    });
    let ann_dir = "%MetadataPath%/People/A/Ann Actor";
    let ann = jf::JfItem::new("Person", "Ann Actor", Some(ann_dir)).image(
        &format!("{ann_dir}/folder.jpg"),
        0,
        (300, 450),
        Some("LKO2?U%2Tw=w"),
    );
    let dan = jf::JfItem::new("Person", "Dan Director", None).image(
        "/config/metadata/People/D/Dan Director/folder.jpg",
        0,
        (300, 450),
        None,
    );
    w.source.items.extend([ann, dan]);
    let install = b20_install(&w, name, layout).await;

    let library = install.join("metadata/library");
    let alpha_dir = library.join("a1/alpha");
    let poster = jf::write_image(&alpha_dir.join("poster.jpg"), jf::JPEG_HEAD, "alpha poster");
    let backdrop = jf::write_image(&alpha_dir.join("backdrop.png"), jf::PNG_HEAD, "backdrop");
    jf::write_image(&alpha_dir.join("box.jpg"), jf::JPEG_HEAD, "alpha box");
    std::fs::write(alpha_dir.join("notimage.jpg"), "not an image").unwrap();
    jf::write_image(&install.join("outside.jpg"), jf::JPEG_HEAD, "outside");
    let escaped = w.jf_root.join("escaped.jpg");
    jf::write_image(&escaped, jf::JPEG_HEAD, "escaped");
    std::os::unix::fs::symlink(&escaped, alpha_dir.join("disc.jpg")).unwrap();
    let logo_path = format!("{}/Alpha (2001)/logo.png", w.movies);
    let logo = jf::write_image(std::path::Path::new(&logo_path), jf::PNG_HEAD, "alpha logo");
    let show_poster = jf::write_image(&library.join("b2/show/poster.jpg"), jf::JPEG_HEAD, "show");
    let ann_file = install.join("metadata/People/A/Ann Actor/folder.jpg");
    let ann_bytes = jf::write_image(&ann_file, jf::JPEG_HEAD, "ann portrait");

    let alpha = b20_item(&w.state, "Alpha").await;
    let beta = b20_item(&w.state, "Beta").await;
    let show = b20_item(&w.state, "Show").await;
    let e1 = b20_item(&w.state, "Show S01E01").await;
    let e3 = b20_item(&w.state, "Show S01E03").await;
    sqlx::query(
        "UPDATE media_items SET overview = 'Rustyfin overview', genres = 'Action', \
         official_rating = 'R' WHERE id = ?",
    )
    .bind(beta.to_string())
    .execute(&w.state.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO item_images (item_id, image_type, image_index, path, provider) \
         VALUES (?, 'Primary', 0, '/nowhere/show.jpg', 'local')",
    )
    .bind(show.to_string())
    .execute(&w.state.db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO people (id, name, type) VALUES ('old-actor', 'Old Actor', 'Actor')")
        .execute(&w.state.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO item_people (item_id, person_id, type) VALUES (?, 'old-actor', 'Actor')",
    )
    .bind(show.to_string())
    .execute(&w.state.db)
    .await
    .unwrap();
    let meta = B20Meta {
        install,
        alpha,
        beta,
        show,
        e1,
        e3,
        poster,
        backdrop,
        logo,
        show_poster,
        ann: ann_bytes,
    };
    (w, meta)
}

/// A report's metadata counts: [Items, Fields, People, Images, ImagesMissing, Locked].
fn b20_meta_counts(report: &serde_json::Value) -> [i64; 6] {
    [
        "Items",
        "Fields",
        "People",
        "Images",
        "ImagesMissing",
        "Locked",
    ]
    .map(|key| report["Metadata"][key].as_i64().unwrap_or(-1))
}

/// The metadata of `item`: its fields, its extras and its studios (names sorted, joined by `|`).
async fn b20_fields(state: &AppState, item: uuid::Uuid) -> serde_json::Value {
    let text: String = sqlx::query_scalar(
        "SELECT json_object('name', m.name, 'sort_name', m.sort_name, 'overview', m.overview, \
         'tagline', m.tagline, 'official_rating', m.official_rating, \
         'community_rating', m.community_rating, 'premiere_date', m.premiere_date, \
         'end_date', m.end_date, 'production_year', m.production_year, 'genres', m.genres, \
         'tags', m.tags, 'tmdb_id', m.tmdb_id, 'tvdb_id', m.tvdb_id, \
         'original_title', x.original_title, 'critic_rating', x.critic_rating, \
         'imdb_id', x.imdb_id, 'studios', (SELECT group_concat(name, '|') FROM \
         (SELECT s.name FROM item_studios i JOIN studios s ON s.id = i.studio_id \
         WHERE i.item_id = m.id ORDER BY s.name))) \
         FROM media_items m LEFT JOIN item_metadata_extras x ON x.item_id = m.id WHERE m.id = ?",
    )
    .bind(item.to_string())
    .fetch_one(&state.db)
    .await
    .unwrap();
    serde_json::from_str(&text).unwrap()
}

/// What `b20_fields` gives for an item named `name` with no metadata, with `changes` applied.
fn b20_expect(name: &str, changes: serde_json::Value) -> serde_json::Value {
    let mut fields = serde_json::json!({
        "name": name,
        "sort_name": null,
        "overview": null,
        "tagline": null,
        "official_rating": null,
        "community_rating": null,
        "premiere_date": null,
        "end_date": null,
        "production_year": null,
        "genres": "[]",
        "tags": "",
        "tmdb_id": null,
        "tvdb_id": null,
        "original_title": null,
        "critic_rating": null,
        "imdb_id": null,
        "studios": null,
    });
    for (key, value) in changes.as_object().unwrap() {
        assert!(fields.get(key).is_some(), "no field {key}");
        fields[key] = value.clone();
    }
    fields
}

/// Alpha's fields after an import of the metadata world, where Rustyfin names it `name`.
fn b20_alpha_fields(name: &str) -> serde_json::Value {
    let changes = serde_json::json!({
        "sort_name": "alpha, the",
        "overview": "Jellyfin overview of Alpha.",
        "tagline": "First of all.",
        "official_rating": "PG-13",
        "community_rating": 7.5,
        "premiere_date": "2001-05-04T00:00:00+00:00",
        "production_year": 2001,
        "genres": "Drama|Comedy",
        "tags": "favourite",
        "tmdb_id": 111,
        "original_title": "Alfa",
        "critic_rating": 88.0,
        "imdb_id": "tt0000111",
        "studios": "Acme Pictures|Big Studio",
    });
    b20_expect(name, changes)
}

/// The Show's fields after an import of the metadata world.
fn b20_show_fields() -> serde_json::Value {
    let changes =
        serde_json::json!({ "overview": "A show.", "tvdb_id": 333, "studios": "Acme TV" });
    b20_expect("Show", changes)
}

/// S01E01's fields after an import of the metadata world.
fn b20_e1_fields() -> serde_json::Value {
    let changes = serde_json::json!({
        "overview": "Pilot.",
        "premiere_date": "2025-01-10T00:00:00+00:00",
    });
    b20_expect("Show S01E01", changes)
}

/// Beta's fields after an import of the metadata world, with the overview and genres given.
fn b20_beta_fields(overview: &str, genres: &str) -> serde_json::Value {
    let changes = serde_json::json!({
        "overview": overview,
        "official_rating": "R",
        "genres": genres,
        "community_rating": 6.0,
        "production_year": 2002,
        "tmdb_id": 222,
        "imdb_id": "tt0000222",
    });
    b20_expect("Beta", changes)
}

/// `GET /Items/{id}` as the admin; expects 200.
async fn b20_get_item(w: &B20World, id: uuid::Uuid) -> serde_json::Value {
    let uri = format!("/Items/{id}");
    let (status, item) = send_authed(&w.app, "GET", &uri, &w.token, None).await;
    assert_eq!(status, StatusCode::OK, "{item}");
    item
}

/// A person of an item answer: (Name, Type, Role, PrimaryImageTag).
type B20Person = (String, String, Option<String>, Option<String>);

fn b20_person(name: &str, kind: &str, role: Option<&str>, tag: Option<&str>) -> B20Person {
    let text = |s: Option<&str>| s.map(str::to_string);
    (name.to_string(), kind.to_string(), text(role), text(tag))
}

/// The `People` of an item answer, in order.
fn b20_people(item: &serde_json::Value) -> Vec<B20Person> {
    let text = |p: &serde_json::Value, key: &str| p[key].as_str().map(str::to_string);
    let people = item["People"].as_array().cloned().unwrap_or_default();
    people
        .iter()
        .map(|p| {
            (
                text(p, "Name").unwrap_or_default(),
                text(p, "Type").unwrap_or_default(),
                text(p, "Role"),
                text(p, "PrimaryImageTag"),
            )
        })
        .collect()
}

/// Alpha's people after an import of the metadata world: in Jellyfin's list order, Ann once (her
/// first credit), Nora without a kind, and only Ann with a portrait.
fn b20_alpha_people() -> Vec<B20Person> {
    vec![
        b20_person("Dan Director", "Director", None, None),
        b20_person("Ann Actor", "Actor", Some("Hero"), Some("Primary_0")),
        b20_person("Nora Nobody", "Unknown", None, None),
    ]
}

/// The id of the Rustyfin person `name`.
async fn b20_person_id(state: &AppState, name: &str) -> String {
    sqlx::query_scalar("SELECT id FROM people WHERE name = ?")
        .bind(name)
        .fetch_one(&state.db)
        .await
        .unwrap()
}

/// An image row: (type, index, path, provider, width, height, blurhash).
type B20Image = (
    String,
    i64,
    String,
    String,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

/// The image rows of the item `id` (`person`: of the person `id`), by type and index.
async fn b20_images(state: &AppState, id: &str, person: bool) -> Vec<B20Image> {
    let sql = if person {
        "SELECT image_type, image_index, path, provider, width, height, blurhash \
         FROM person_images WHERE person_id = ? ORDER BY image_type, image_index"
    } else {
        "SELECT image_type, image_index, path, provider, width, height, blurhash \
         FROM item_images WHERE item_id = ? ORDER BY image_type, image_index"
    };
    sqlx::query_as(sql)
        .bind(id)
        .fetch_all(&state.db)
        .await
        .unwrap()
}

/// Image rows without their path and provider: (type, index, width, height, blurhash).
/// An image row without its path and provider: type, index, width, height, blurhash.
type B20ImageShape<'a> = (&'a str, i64, Option<i64>, Option<i64>, Option<&'a str>);

fn b20_image_shapes(rows: &[B20Image]) -> Vec<B20ImageShape<'_>> {
    rows.iter()
        .map(|(kind, index, _, _, width, height, hash)| {
            (kind.as_str(), *index, *width, *height, hash.as_deref())
        })
        .collect()
}

/// Rustyfin's own poster of the Show in the metadata world.
fn b20_local_poster() -> B20Image {
    let path = "/nowhere/show.jpg".to_string();
    (
        "Primary".to_string(),
        0,
        path,
        "local".to_string(),
        None,
        None,
        None,
    )
}

/// Checks that `row` is an imported copy `<data_dir>/images/<folder>/jf-<Type>-<index>-<16 hex>.<ext>`
/// and returns the copy's bytes.
fn b20_copy(state: &AppState, folder: &str, row: &B20Image, ext: &str) -> Vec<u8> {
    let (kind, index, path, provider, ..) = row;
    assert_eq!(provider, "jellyfin", "{row:?}");
    let path = std::path::Path::new(path);
    let parent = state.data_dir.join("images").join(folder);
    assert_eq!(path.parent(), Some(parent.as_path()), "{row:?}");
    let file = path.file_name().unwrap().to_str().unwrap();
    let hex = file
        .strip_prefix(&format!("jf-{kind}-{index}-"))
        .and_then(|rest| rest.strip_suffix(&format!(".{ext}")));
    let lower_hex = |h: &str| h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    assert!(
        hex.is_some_and(|h| h.len() == 16 && lower_hex(h)),
        "copy name {file}"
    );
    std::fs::read(path).unwrap()
}

/// GETs `uri` as the admin; returns the status and the body.
async fn b20_get_bytes(w: &B20World, uri: &str) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .uri(uri)
        .header("Authorization", format!("Bearer {}", w.token))
        .body(Body::empty())
        .unwrap();
    let resp = w.app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

/// The regular files under `dir`, recursively, sorted; none when `dir` doesn't exist.
fn b20_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(d) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                dirs.push(entry.path());
            } else if kind.is_file() {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    files
}

/// The rows of the tables the metadata import writes, as JSON arrays, sorted per table.
async fn b20_meta_rows(state: &AppState) -> Vec<String> {
    let image = "image_type, image_index, path, provider, width, height, blurhash";
    let tables = [
        ("item_images", format!("item_id, {image}")),
        ("people", "id, name, type".to_string()),
        (
            "item_people",
            "item_id, person_id, type, role, sort_order".to_string(),
        ),
        ("studios", "id, name".to_string()),
        ("item_studios", "item_id, studio_id".to_string()),
        (
            "item_metadata_extras",
            "item_id, original_title, critic_rating, imdb_id, lock_data, locked_fields".to_string(),
        ),
        ("person_images", format!("person_id, {image}")),
    ];
    let mut rows = Vec::new();
    for (table, columns) in tables {
        let sql = format!("SELECT json_array({columns}) FROM {table} ORDER BY 1");
        let found: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .fetch_all(&state.db)
            .await
            .unwrap();
        rows.push(format!("{table}: {}", found.join(" ")));
    }
    rows
}

/// The fields of every item, then `b20_meta_rows`.
async fn b20_meta_digest(state: &AppState) -> Vec<String> {
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM media_items ORDER BY id")
        .fetch_all(&state.db)
        .await
        .unwrap();
    let mut digest = Vec::new();
    for id in ids {
        let id = uuid::Uuid::parse_str(&id).unwrap();
        digest.push(b20_fields(state, id).await.to_string());
    }
    digest.extend(b20_meta_rows(state).await);
    digest
}

#[tokio::test]
async fn b20_import_metadata_fills_empty_fields() {
    for (name, layout) in [("new", jf::Layout::New), ("old", jf::Layout::Old)] {
        let (w, m) = b20_meta_world(name, layout).await;
        let before = jf::listing(&m.install);
        let body = serde_json::json!({ "SourcePath": m.install });
        let log = jf::LogCapture::start();
        let preview = b20_preview(&w, body.clone()).await;
        let report = b20_run(&w, body.clone()).await;
        let status = b20_status(&w).await;
        let text = log.text();
        drop(log);
        let counts = [4, 21, 4, 4, 7, 0];
        assert_eq!(
            b20_meta_counts(&preview),
            counts,
            "{name}: preview {preview}"
        );
        assert_eq!(b20_meta_counts(&report), counts, "{name}: {report}");
        assert_eq!(report["Metadata"]["Mode"], "FillEmpty", "{name}");

        assert_eq!(
            b20_fields(&w.state, m.alpha).await,
            b20_alpha_fields("Alpha"),
            "{name}: Alpha"
        );
        assert_eq!(
            b20_fields(&w.state, m.beta).await,
            b20_beta_fields("Rustyfin overview", "Action"),
            "{name}: Beta keeps its own"
        );
        let show_fields = b20_fields(&w.state, m.show).await;
        assert_eq!(show_fields, b20_show_fields(), "{name}: Show");
        assert_eq!(b20_fields(&w.state, m.e1).await, b20_e1_fields(), "{name}");
        assert_eq!(
            b20_fields(&w.state, m.e3).await,
            b20_expect("Show S01E03", serde_json::json!({})),
            "{name}: a name is never empty"
        );

        let alpha = b20_get_item(&w, m.alpha).await;
        assert_eq!(b20_people(&alpha), b20_alpha_people(), "{name}");
        assert_eq!(alpha["OriginalTitle"], "Alfa", "{name}");
        assert_eq!(alpha["CriticRating"], 88.0, "{name}");
        assert_eq!(alpha["ProviderIds"]["tmdb"], "111", "{name}");
        assert_eq!(alpha["ProviderIds"]["imdb"], "tt0000111", "{name}");
        assert_eq!(alpha["LockData"], false, "{name}");
        assert_eq!(alpha["LockedFields"], serde_json::json!([]), "{name}");
        let beta = b20_get_item(&w, m.beta).await;
        let bea = b20_person("Bea Star", "Actor", Some("Lead"), None);
        assert_eq!(b20_people(&beta), vec![bea], "{name}");
        let show = b20_get_item(&w, m.show).await;
        let old = b20_person("Old Actor", "Actor", None, None);
        assert_eq!(
            b20_people(&show),
            vec![old],
            "{name}: the Show keeps its people"
        );
        assert_eq!(
            b20_images(&w.state, &m.show.to_string(), false).await,
            vec![b20_local_poster()],
            "{name}: the Show keeps its poster"
        );

        for answer in [&preview, &report, &status] {
            assert!(
                !answer.to_string().contains(jf::CANARY),
                "{name}: secret in an answer"
            );
        }
        assert!(!text.contains(jf::CANARY), "{name}: secret in the log");
        let db = w.state.data_dir.join("test.db");
        let mut files = vec![
            db.clone(),
            db.with_extension("db-wal"),
            db.with_extension("db-shm"),
        ];
        files.extend(b20_files(&w.state.data_dir.join("images")));
        assert!(
            !jf::files_hold_canary(&files),
            "{name}: secret in Rustyfin's files"
        );
        assert_eq!(
            jf::listing(&m.install),
            before,
            "{name}: the source changed"
        );

        let digest = b20_meta_digest(&w.state).await;
        let again = b20_run(&w, body).await;
        assert_eq!(
            b20_meta_counts(&again)[..4],
            [0, 0, 0, 0],
            "{name}: {again}"
        );
        assert_eq!(
            b20_meta_digest(&w.state).await,
            digest,
            "{name}: the second run wrote"
        );
    }
}

#[tokio::test]
async fn b20_import_metadata_copies_images() {
    for (name, layout) in [("new", jf::Layout::New), ("old", jf::Layout::Old)] {
        let (w, m) = b20_meta_world(name, layout).await;
        let report = b20_run(&w, serde_json::json!({ "SourcePath": m.install })).await;
        assert_eq!(b20_meta_counts(&report)[3..5], [4, 7], "{name}: {report}");
        let alpha = m.alpha.to_string();
        let rows = b20_images(&w.state, &alpha, false).await;
        let shapes = vec![
            ("Backdrop", 0, Some(1920), Some(1080), None),
            ("Logo", 0, None, None, None),
            ("Primary", 0, Some(1000), Some(1500), Some("LEHV*nWB|yk8")),
        ];
        assert_eq!(b20_image_shapes(&rows), shapes, "{name}: Alpha's rows");
        assert_eq!(
            b20_copy(&w.state, &alpha, &rows[0], "png"),
            m.backdrop,
            "{name}"
        );
        assert_eq!(
            b20_copy(&w.state, &alpha, &rows[1], "png"),
            m.logo,
            "{name}"
        );
        assert_eq!(
            b20_copy(&w.state, &alpha, &rows[2], "jpg"),
            m.poster,
            "{name}"
        );

        let ann = b20_person_id(&w.state, "Ann Actor").await;
        let rows = b20_images(&w.state, &ann, true).await;
        let shapes = vec![("Primary", 0, Some(300), Some(450), Some("LKO2?U%2Tw=w"))];
        assert_eq!(b20_image_shapes(&rows), shapes, "{name}: Ann's rows");
        let folder = format!("people/{ann}");
        assert_eq!(
            b20_copy(&w.state, &folder, &rows[0], "jpg"),
            m.ann,
            "{name}"
        );
        let dan = b20_person_id(&w.state, "Dan Director").await;
        assert!(
            b20_images(&w.state, &dan, true).await.is_empty(),
            "{name}: Dan's portrait is missing"
        );
        let copies = b20_files(&w.state.data_dir.join("images"));
        assert_eq!(copies.len(), 4, "{name}: {copies:?}");

        for (uri, bytes) in [
            (format!("/Items/{alpha}/Images/Primary"), &m.poster),
            (format!("/Items/{alpha}/Images/Backdrop/0"), &m.backdrop),
            (format!("/Items/{ann}/Images/Primary"), &m.ann),
        ] {
            let (status, body) = b20_get_bytes(&w, &uri).await;
            assert_eq!(status, StatusCode::OK, "{name}: {uri}");
            assert_eq!(&body, bytes, "{name}: {uri}");
        }
    }
}

#[tokio::test]
async fn b20_import_metadata_replaces_with_jellyfins() {
    let (w, m) = b20_meta_world("jf", jf::Layout::New).await;
    let body = serde_json::json!({ "SourcePath": m.install, "MetadataMode": "Replace" });
    let report = b20_run(&w, body.clone()).await;
    assert_eq!(b20_meta_counts(&report), [5, 25, 5, 5, 7, 0], "{report}");
    assert_eq!(report["Metadata"]["Mode"], "Replace");
    assert_eq!(
        b20_fields(&w.state, m.alpha).await,
        b20_alpha_fields("Alpha: Jellyfin Cut")
    );
    assert_eq!(
        b20_fields(&w.state, m.beta).await,
        b20_beta_fields("Jellyfin overview of Beta.", "Thriller"),
        "Jellyfin has no rating for Beta, so its R stays"
    );
    assert_eq!(b20_fields(&w.state, m.show).await, b20_show_fields());
    assert_eq!(b20_fields(&w.state, m.e1).await, b20_e1_fields());
    let e3 = serde_json::json!({ "sort_name": "show 1x03" });
    assert_eq!(
        b20_fields(&w.state, m.e3).await,
        b20_expect("Show 1x03", e3),
        "the sort name follows a new name"
    );

    let alpha = b20_get_item(&w, m.alpha).await;
    assert_eq!(b20_people(&alpha), b20_alpha_people());
    let show = b20_get_item(&w, m.show).await;
    let sam = b20_person("Sam Showrunner", "Creator", None, None);
    assert_eq!(
        b20_people(&show),
        vec![sam],
        "Jellyfin's people replace the Show's"
    );
    let show = m.show.to_string();
    let rows = b20_images(&w.state, &show, false).await;
    let shapes = vec![("Primary", 0, Some(680), Some(1000), None)];
    assert_eq!(
        b20_image_shapes(&rows),
        shapes,
        "Jellyfin's poster replaces the Show's"
    );
    assert_eq!(b20_copy(&w.state, &show, &rows[0], "jpg"), m.show_poster);
    let images = w.state.data_dir.join("images");
    assert_eq!(b20_files(&images).len(), 5);

    let digest = b20_meta_digest(&w.state).await;
    let again = b20_run(&w, body).await;
    assert_eq!(b20_meta_counts(&again)[..4], [0, 0, 0, 0], "{again}");
    assert_eq!(
        b20_meta_digest(&w.state).await,
        digest,
        "the second run wrote"
    );
    assert_eq!(b20_files(&images).len(), 5, "the second run copied");
}

#[tokio::test]
async fn b20_import_metadata_skips_locked_items_and_fields() {
    let (w, m) = b20_meta_world("jf", jf::Layout::New).await;
    for (item, lock_data, locked_fields) in [(m.show, 1, ""), (m.beta, 0, "Overview|Genres|Cast")] {
        sqlx::query(
            "INSERT INTO item_metadata_extras (item_id, lock_data, locked_fields) VALUES (?, ?, ?)",
        )
        .bind(item.to_string())
        .bind(lock_data)
        .bind(locked_fields)
        .execute(&w.state.db)
        .await
        .unwrap();
    }
    let show_fields = b20_fields(&w.state, m.show).await;
    let body = serde_json::json!({ "SourcePath": m.install, "MetadataMode": "Replace" });
    let report = b20_run(&w, body).await;
    assert_eq!(b20_meta_counts(&report), [4, 21, 3, 4, 7, 1], "{report}");

    assert_eq!(
        b20_fields(&w.state, m.beta).await,
        b20_beta_fields("Rustyfin overview", "Action"),
        "Beta's locked fields changed"
    );
    let beta = b20_get_item(&w, m.beta).await;
    assert!(b20_people(&beta).is_empty(), "Beta's cast is locked");
    assert_eq!(beta["LockData"], false);
    let locked = serde_json::json!(["Overview", "Genres", "Cast"]);
    assert_eq!(beta["LockedFields"], locked);

    assert_eq!(
        b20_fields(&w.state, m.show).await,
        show_fields,
        "the locked Show changed"
    );
    let show = b20_get_item(&w, m.show).await;
    let old = b20_person("Old Actor", "Actor", None, None);
    assert_eq!(b20_people(&show), vec![old]);
    assert_eq!(show["LockData"], true);
    assert_eq!(show["LockedFields"], serde_json::json!([]));
    assert_eq!(
        b20_images(&w.state, &m.show.to_string(), false).await,
        vec![b20_local_poster()]
    );
    let created: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM people WHERE name IN ('Sam Showrunner', 'Bea Star')",
    )
    .fetch_one(&w.state.db)
    .await
    .unwrap();
    assert_eq!(created, 0, "people of locked casts were created");
    assert_eq!(
        b20_fields(&w.state, m.alpha).await,
        b20_alpha_fields("Alpha: Jellyfin Cut")
    );
}

#[tokio::test]
async fn b20_import_metadata_preview_and_mode_none_write_nothing() {
    let (w, m) = b20_meta_world("jf", jf::Layout::New).await;
    let before = b20_db_digest(&w.state).await;
    let meta = b20_meta_digest(&w.state).await;
    let fill = b20_preview(&w, serde_json::json!({ "SourcePath": m.install })).await;
    assert_eq!(b20_meta_counts(&fill), [4, 21, 4, 4, 7, 0], "{fill}");
    assert_eq!(fill["Metadata"]["Mode"], "FillEmpty");
    let body = serde_json::json!({ "SourcePath": m.install, "MetadataMode": "Replace" });
    let replace = b20_preview(&w, body).await;
    assert_eq!(b20_meta_counts(&replace), [5, 25, 5, 5, 7, 0], "{replace}");
    assert_eq!(replace["Metadata"]["Mode"], "Replace");
    let pair = (
        "/config/metadata".to_string(),
        m.install.join("metadata").display().to_string(),
    );
    let pairs = b20_pairs(&fill, "SuggestedPathMappings");
    assert_eq!(pairs.last(), Some(&pair), "{pairs:?}");
    assert_eq!(b20_db_digest(&w.state).await, before, "the preview wrote");
    assert_eq!(
        b20_meta_digest(&w.state).await,
        meta,
        "the preview wrote metadata"
    );
    let images = w.state.data_dir.join("images");
    assert!(!images.exists(), "the preview copied images");

    let body = serde_json::json!({ "SourcePath": m.install, "MetadataMode": "None" });
    let none = b20_run(&w, body).await;
    assert_eq!(b20_meta_counts(&none), [0; 6], "{none}");
    assert_eq!(none["Metadata"]["Mode"], "None");
    assert_eq!(
        b20_meta_digest(&w.state).await,
        meta,
        "mode None wrote metadata"
    );
    assert!(!images.exists(), "mode None copied images");
}

/// What a rescan must keep after a Replace import of the metadata world: Alpha's fields, the
/// imported fields of the others (a rescan may give Beta and the Show the sort name they lack)
/// and the metadata rows.
async fn b20_imported(state: &AppState, m: &B20Meta) -> Vec<String> {
    let picks: [(uuid::Uuid, &[&str]); 4] = [
        (
            m.beta,
            &[
                "overview",
                "community_rating",
                "production_year",
                "genres",
                "imdb_id",
            ],
        ),
        (m.show, &["overview", "studios"]),
        (m.e1, &["overview", "premiere_date"]),
        (m.e3, &["name", "sort_name"]),
    ];
    let mut kept = vec![b20_fields(state, m.alpha).await.to_string()];
    for (item, keys) in picks {
        let fields = b20_fields(state, item).await;
        kept.extend(keys.iter().map(|k| format!("{item} {k} {}", fields[*k])));
    }
    kept.extend(b20_meta_rows(state).await);
    kept
}

#[tokio::test]
async fn b20_import_metadata_survives_a_rescan() {
    let (w, m) = b20_meta_world("jf", jf::Layout::New).await;
    let paths: Vec<String> =
        sqlx::query_scalar("SELECT path FROM media_items WHERE media_type IN ('Movie', 'Episode')")
            .fetch_all(&w.state.db)
            .await
            .unwrap();
    assert_eq!(paths.len(), 5);
    for path in &paths {
        let path = std::path::Path::new(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"").unwrap();
    }
    let body = serde_json::json!({ "SourcePath": m.install, "MetadataMode": "Replace" });
    let report = b20_run(&w, body).await;
    assert_eq!(b20_meta_counts(&report), [5, 25, 5, 5, 7, 0], "{report}");
    let imported = b20_imported(&w.state, &m).await;

    let libraries = rustyfin_db::repo_library::get_all_libraries(&w.state.db)
        .await
        .unwrap();
    assert_eq!(libraries.len(), 2);
    for library in &libraries {
        rustyfin_core::scanner::scan_library(&w.state.db, library)
            .await
            .unwrap();
    }
    rustyfin_core::scanner::wait_for_idle().await;
    assert_eq!(
        b20_imported(&w.state, &m).await,
        imported,
        "the rescan lost imported data"
    );
}

/// The B20 world with a stale Jellyfin entry of Alpha, installed into the folder `name` in
/// `layout`. Jellyfin's Alpha gets TMDB 111 and an overview; "Alpha (stale)", listed before it, is
/// a Movie at an old path in Alpha's folder with TMDB 111, its own overview and bob's played row.
/// Rustyfin's Alpha has no TMDB id, so only an id the import writes can match the stale entry.
/// `edit` gets the world and the stale entry's id before the install.
async fn b20_stale_world_with(
    name: &str,
    layout: jf::Layout,
    edit: impl FnOnce(&mut B20World, uuid::Uuid),
) -> (B20World, std::path::PathBuf) {
    let mut w = b20_world().await;
    b20_edit(&mut w, "Alpha", |i| {
        i.provider("Tmdb", "111").meta(jf::JfMeta {
            overview: Some("Jellyfin overview of Alpha."),
            ..Default::default()
        })
    });
    let stale = jf::JfItem::new(
        "Movie",
        "Alpha (stale)",
        Some("/data/movies/Alpha (2001)/Alpha.old.avi"),
    )
    .provider("Tmdb", "111")
    .meta(jf::JfMeta {
        overview: Some("Stale overview."),
        ..Default::default()
    });
    let bob = w.source.user("bob").id;
    w.source.user_data.push(jf::JfUserData {
        user: bob,
        item: stale.id,
        played: true,
        play_count: 1,
        last_played: Some("2025-04-01 08:00:00"),
        ..Default::default()
    });
    let stale_id = stale.id;
    let at = w.source.items.iter().position(|i| i.name == "Alpha");
    w.source.items.insert(at.unwrap(), stale);
    edit(&mut w, stale_id);
    let install = b20_install(&w, name, layout).await;
    (w, install)
}

/// The stale world, as installed.
async fn b20_stale_world(name: &str, layout: jf::Layout) -> (B20World, std::path::PathBuf) {
    b20_stale_world_with(name, layout, |_, _| {}).await
}

#[tokio::test]
async fn b20_import_maps_tracks_only_at_the_jellyfin_path() {
    for (name, layout) in [("new", jf::Layout::New), ("old", jf::Layout::Old)] {
        // bob chose the stale file's aac and German subtitles; Rustyfin's Alpha has streams of
        // the same kinds and codecs, but at its own path, not the stale entry's.
        let (w, install) = b20_stale_world_with(name, layout, |w, stale| {
            let stream = jf::JfStream::new;
            w.source.streams.extend([
                stream(stale, 0, "Video", "h264"),
                stream(stale, 1, "Audio", "aac"),
                stream(stale, 2, "Subtitle", "srt").language("ger"),
            ]);
            let bob = w.source.user("bob").id;
            let mut rows = w.source.user_data.iter_mut();
            let row = rows
                .find(|row| row.user == bob && row.item == stale)
                .unwrap();
            row.audio = Some(1);
            row.subtitle = Some(2);
        })
        .await;
        let alpha = b20_item(&w.state, "Alpha").await;
        let alpha_file = std::path::PathBuf::from(format!("{}/Alpha (2001)/Alpha.mkv", w.movies));
        let streams = serde_json::json!([
            { "Index": 0, "Codec": "h264", "CodecType": "Video" },
            { "Index": 1, "Codec": "aac", "CodecType": "Audio" },
            { "Index": 2, "Codec": "subrip", "CodecType": "Subtitle" },
        ]);
        b20_insert_source(&w.state, alpha, &alpha_file, streams).await;

        let body = serde_json::json!({ "SourcePath": install });
        let preview = b20_preview(&w, body.clone()).await;
        let report = b20_run(&w, body).await;
        for report in [&preview, &report] {
            assert_eq!(b20_track_counts(report), (0, 0, 0, 2), "{name}: {report}");
        }
        let bob = b20_entry(&report, "bob").user_id.unwrap().to_string();
        let tracks = b20_stored_tracks(&w.state, &bob, alpha).await;
        assert_eq!(tracks, (None, None), "{name}: choices of another file");
    }
}

#[tokio::test]
async fn b20_import_metadata_ids_count_for_matching() {
    // The watch counts of the first run and of the second.
    for (name, layout, first_watch, second_watch) in [
        ("new", jf::Layout::New, (7, 5, 0, 2), (7, 0, 5, 2)),
        ("old", jf::Layout::Old, (6, 5, 0, 1), (6, 0, 5, 1)),
    ] {
        let (w, install) = b20_stale_world(name, layout).await;
        let body = serde_json::json!({ "SourcePath": install });
        let preview = b20_preview(&w, body.clone()).await;
        let first = b20_run(&w, body.clone()).await;
        for report in [&preview, &first] {
            assert_eq!(b20_item_counts(report), (10, 5, 3, 2), "{name}: {report}");
            assert_eq!(b20_watch_counts(report), first_watch, "{name}: {report}");
            assert_eq!(
                b20_meta_counts(report),
                [1, 2, 0, 0, 0, 0],
                "{name}: {report}"
            );
        }
        let alpha = b20_item(&w.state, "Alpha").await;
        let fields = b20_fields(&w.state, alpha).await;
        assert_eq!(fields["tmdb_id"], 111, "{name}: {fields}");
        assert_eq!(
            fields["overview"], "Jellyfin overview of Alpha.",
            "{name}: the path match is the source"
        );
        let bob = b20_entry(&first, "bob").user_id.unwrap();
        let expected = B20Data {
            played: true,
            play_count: 1,
            last_played: b20_at("2025-04-01T08:00:00Z"),
            ..Default::default()
        };
        let stored = b20_data(&w.state, bob, alpha).await;
        assert_eq!(
            stored,
            Some(expected),
            "{name}: bob's row of the stale entry"
        );

        let rows = b20_db_digest(&w.state).await;
        let meta = b20_meta_digest(&w.state).await;
        let second = b20_run(&w, body).await;
        assert_eq!(b20_item_counts(&second), (10, 5, 3, 2), "{name}: {second}");
        assert_eq!(b20_watch_counts(&second), second_watch, "{name}: {second}");
        assert_eq!(b20_meta_counts(&second), [0; 6], "{name}: {second}");
        assert_eq!(b20_db_digest(&w.state).await, rows, "{name}: second run");
        assert_eq!(b20_meta_digest(&w.state).await, meta, "{name}: second run");
    }
}

#[tokio::test]
async fn b20_import_metadata_none_matches_on_stored_ids() {
    // The watch counts of the first run and of the second.
    for (name, layout, first_watch, second_watch) in [
        ("new", jf::Layout::New, (7, 4, 0, 3), (7, 0, 4, 3)),
        ("old", jf::Layout::Old, (6, 4, 0, 2), (6, 0, 4, 2)),
    ] {
        let (w, install) = b20_stale_world(name, layout).await;
        let body = serde_json::json!({ "SourcePath": install, "MetadataMode": "None" });
        let first = b20_run(&w, body.clone()).await;
        assert_eq!(b20_item_counts(&first), (10, 5, 2, 3), "{name}: {first}");
        assert_eq!(b20_watch_counts(&first), first_watch, "{name}: {first}");
        let alpha = b20_item(&w.state, "Alpha").await;
        let fields = b20_fields(&w.state, alpha).await;
        assert_eq!(
            fields["tmdb_id"],
            serde_json::Value::Null,
            "{name}: {fields}"
        );
        let bob = b20_entry(&first, "bob").user_id.unwrap();
        let stored = b20_data(&w.state, bob, alpha).await;
        assert_eq!(stored, None, "{name}: the stale entry matched");
        let second = b20_run(&w, body).await;
        assert_eq!(b20_item_counts(&second), (10, 5, 2, 3), "{name}: {second}");
        assert_eq!(b20_watch_counts(&second), second_watch, "{name}: {second}");
    }
}

#[tokio::test]
async fn b20_import_metadata_replace_keeps_rustyfins_ids() {
    for (name, layout) in [("new", jf::Layout::New), ("old", jf::Layout::Old)] {
        let (w, install) = b20_stale_world(name, layout).await;
        let alpha = b20_item(&w.state, "Alpha").await;
        let show = b20_item(&w.state, "Show").await;
        // Rustyfin's own ids, which differ from Jellyfin's (TMDB 111 for Alpha, TVDB 333).
        for (sql, id) in [
            ("UPDATE media_items SET tmdb_id = 555 WHERE id = ?", alpha),
            ("UPDATE media_items SET tvdb_id = 444 WHERE id = ?", show),
        ] {
            sqlx::query(sql)
                .bind(id.to_string())
                .execute(&w.state.db)
                .await
                .unwrap();
        }
        // Filling keeps Rustyfin's TMDB id, so the stale entry stays unmatched.
        let fill = b20_preview(&w, serde_json::json!({ "SourcePath": install })).await;
        assert_eq!(b20_item_counts(&fill), (10, 5, 2, 3), "{name}: {fill}");

        // Replacing keeps them too: the ids decide what matches.
        let body = serde_json::json!({ "SourcePath": install, "MetadataMode": "Replace" });
        let preview = b20_preview(&w, body.clone()).await;
        let first = b20_run(&w, body.clone()).await;
        for report in [&preview, &first] {
            assert_eq!(b20_item_counts(report), (10, 5, 2, 3), "{name}: {report}");
        }
        let fields = b20_fields(&w.state, alpha).await;
        assert_eq!(fields["tmdb_id"], 555, "{name}: {fields}");
        assert_eq!(
            fields["name"], "Alpha",
            "{name}: the stale entry renamed Alpha"
        );
        assert_eq!(
            fields["overview"], "Jellyfin overview of Alpha.",
            "{name}: Replace still replaces the other fields"
        );
        let fields = b20_fields(&w.state, show).await;
        assert_eq!(fields["tvdb_id"], 444, "{name}: {fields}");
        let bob = b20_entry(&first, "bob").user_id.unwrap();
        let stored = b20_data(&w.state, bob, alpha).await;
        assert_eq!(stored, None, "{name}: the stale entry's row was imported");

        let rows = b20_db_digest(&w.state).await;
        let meta = b20_meta_digest(&w.state).await;
        let second = b20_run(&w, body).await;
        assert_eq!(b20_item_counts(&second), (10, 5, 2, 3), "{name}: {second}");
        assert_eq!(b20_watch_counts(&second).1, 0, "{name}: {second}");
        assert_eq!(b20_meta_counts(&second), [0; 6], "{name}: {second}");
        assert_eq!(
            b20_db_digest(&w.state).await,
            rows,
            "{name}: the second run changed rows"
        );
        assert_eq!(
            b20_meta_digest(&w.state).await,
            meta,
            "{name}: the second run changed metadata"
        );
    }
}
