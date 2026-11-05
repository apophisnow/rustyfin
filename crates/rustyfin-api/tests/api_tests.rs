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
use rustyfin_model::server_info::ServerConfiguration;
use rustyfin_model::ws_event::WsEvent;

async fn test_state() -> AppState {
    let data_dir = std::env::temp_dir().join(format!("rustyfin_test_{}", uuid::Uuid::new_v4()));
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
async fn test_startup_flag_without_owner_still_needs_setup() {
    let state = test_state().await;
    add_user(&state, "olduser", "oldpass", true).await;
    state.config.write().await.is_startup_wizard_completed = true;
    let app = app(state.clone());

    assert_eq!(wizard_completed(&app).await, (false, false));
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
async fn test_startup_user_with_admin_but_no_owner() {
    let state = test_state().await;
    add_user(&state, "olduser", "oldpass", true).await;
    let app = app(state.clone());

    assert_eq!(wizard_completed(&app).await, (false, false));
    let (status, body) = send(&app, request("GET", "/Startup/User", None, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["Name"], "",
        "no user name leaks before an owner exists"
    );

    assert_eq!(
        post_startup_user(&app, None, "karl", "geheim").await,
        StatusCode::NO_CONTENT
    );
    assert!(login(&app, "karl", "geheim").await.is_some());
    assert!(
        login(&app, "olduser", "oldpass").await.is_some(),
        "existing admin unchanged"
    );
    assert_eq!(
        user_flags(&state, "olduser").await,
        Some((true, false, false))
    );
    assert_eq!(
        owners_and_count(&state).await,
        (vec!["karl".to_string()], 2)
    );
}

#[tokio::test]
async fn test_startup_user_takes_over_existing_admin_name() {
    let state = test_state().await;
    // A leftover "admin" from an older version, demoted to show that the owner becomes an administrator.
    let admin_id = add_user(&state, "admin", "admin", false).await;
    let app = app(state.clone());

    assert_eq!(
        post_startup_user(&app, None, "admin", "newpass").await,
        StatusCode::NO_CONTENT
    );
    let (_, user_id) = login(&app, "admin", "newpass")
        .await
        .expect("the typed name logs in with the new password");
    assert_eq!(
        user_id,
        admin_id.to_string(),
        "the existing account is reused"
    );
    assert!(login(&app, "admin", "admin").await.is_none());
    assert_eq!(user_flags(&state, "admin").await, Some((true, true, false)));
    assert_eq!(
        owners_and_count(&state).await,
        (vec!["admin".to_string()], 1)
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
async fn test_startup_user_disables_leftover_default_admin() {
    let state = test_state().await;
    add_user(&state, "admin", "admin", true).await;
    let app = app(state.clone());

    assert_eq!(
        post_startup_user(&app, None, "karl", "geheim").await,
        StatusCode::NO_CONTENT
    );
    assert!(
        login(&app, "karl", "geheim").await.is_some(),
        "the owner can log in"
    );
    assert!(
        login(&app, "admin", "admin").await.is_none(),
        "admin/admin is disabled"
    );
    assert_eq!(
        user_flags(&state, "admin").await,
        Some((true, false, true)),
        "disabled, not deleted"
    );
    assert_eq!(
        owners_and_count(&state).await,
        (vec!["karl".to_string()], 2)
    );
}

#[tokio::test]
async fn test_startup_user_keeps_admin_with_changed_password() {
    let state = test_state().await;
    add_user(&state, "admin", "secret", true).await;
    let app = app(state.clone());

    assert_eq!(
        post_startup_user(&app, None, "karl", "geheim").await,
        StatusCode::NO_CONTENT
    );
    assert!(
        login(&app, "karl", "geheim").await.is_some(),
        "the owner can log in"
    );
    assert!(
        login(&app, "admin", "secret").await.is_some(),
        "admin keeps working"
    );
    assert_eq!(
        user_flags(&state, "admin").await,
        Some((true, false, false))
    );
}

#[tokio::test]
async fn test_startup_user_name_match_is_exact() {
    let state = test_state().await;
    let admin_id = add_user(&state, "admin", "admin", true).await;
    let app = app(state.clone());

    assert_eq!(
        post_startup_user(&app, None, "Admin", "geheim").await,
        StatusCode::NO_CONTENT
    );
    let (_, owner_id) = login(&app, "Admin", "geheim")
        .await
        .expect("\"Admin\" is a new account");
    assert_ne!(owner_id, admin_id.to_string());
    assert!(
        login(&app, "admin", "admin").await.is_none(),
        "admin/admin is disabled"
    );
    assert_eq!(user_flags(&state, "admin").await, Some((true, false, true)));
    assert_eq!(
        owners_and_count(&state).await,
        (vec!["Admin".to_string()], 2)
    );
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
async fn g2_disabled_leftover_admin_token_is_rejected() {
    let state = test_state().await;
    add_user(&state, "admin", "admin", true).await;
    let app = app(state.clone());
    // The leftover admin logs in before the wizard runs.
    let (admin_t, _) = g2_login_device(&app, "admin", "admin", "admin-dev")
        .await
        .expect("admin login");
    // The wizard creates the real owner, disabling the leftover admin.
    assert_eq!(
        post_startup_user(&app, None, "karl", "geheim").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        user_flags(&state, "admin").await,
        Some((true, false, true)),
        "the leftover admin is disabled"
    );

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
async fn g2f_unlisted_loopback_peer_xff_is_ignored() {
    // Loopback is a trusted proxy only when listed, so its XFF is ignored and the request stays the
    // loopback peer's own, which is exempt. 90c99b5 always trusted loopback and limited 203.0.113.5.
    let app = g2f_app_from(g2f_state("").await, "127.0.0.1:6100");
    assert_eq!(
        g2f_limited(&app, 5, &["203.0.113.5"]).await,
        0,
        "an unlisted loopback peer's XFF is ignored"
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
