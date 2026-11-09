use chrono::Utc;
use sqlx::SqlitePool;
use std::path::PathBuf;
use uuid::Uuid;

use rustyfin_model::library::{Library, LibraryContentType};
use rustyfin_model::media::{MediaItem, MediaType};
use rustyfin_model::user::User;

/// Create a temporary file-based SQLite database for proper test isolation.
/// Returns (pool, temp_dir) — the temp dir is deleted when dropped.
struct TempDb {
    pool: SqlitePool,
    _dir: PathBuf,
}

impl TempDb {
    async fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("rustyfin_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("test.db");
        let db_url = db_path.to_str().unwrap().to_string();
        let pool = rustyfin_db::init_db(&db_url).await.unwrap();
        Self { pool, _dir: dir }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self._dir);
    }
}

/// Create an in-memory SQLite database with all migrations applied.
async fn test_pool() -> TempDb {
    TempDb::new().await
}

fn make_user(name: &str, is_admin: bool) -> User {
    User {
        id: Uuid::new_v4(),
        name: name.to_string(),
        password_hash: "$argon2id$v=19$m=19456,t=2,p=1$fake$fakehash".to_string(),
        is_administrator: is_admin,
        created_at: Utc::now(),
        last_login_date: None,
        configuration: serde_json::json!({}),
    }
}

fn make_library(name: &str, ct: LibraryContentType) -> Library {
    Library {
        id: Uuid::new_v4(),
        name: name.to_string(),
        content_type: ct,
        paths: vec!["/media/movies".to_string()],
    }
}

// ── User Repository Tests ──

#[tokio::test]
async fn test_create_and_get_user_by_name() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let user = make_user("alice", true);
    rustyfin_db::repo_user::create_user(&pool, &user)
        .await
        .unwrap();

    let found = rustyfin_db::repo_user::get_user_by_name(&pool, "alice")
        .await
        .unwrap();
    assert!(found.is_some());
    let found = found.unwrap();
    assert_eq!(found.name, "alice");
    assert!(found.is_administrator);
    assert_eq!(found.id, user.id);
}

#[tokio::test]
async fn test_get_user_by_id() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let user = make_user("bob", false);
    rustyfin_db::repo_user::create_user(&pool, &user)
        .await
        .unwrap();

    let found = rustyfin_db::repo_user::get_user_by_id(&pool, user.id)
        .await
        .unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().name, "bob");
}

#[tokio::test]
async fn test_get_user_not_found() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let found = rustyfin_db::repo_user::get_user_by_name(&pool, "nonexistent")
        .await
        .unwrap();
    assert!(found.is_none());

    let found = rustyfin_db::repo_user::get_user_by_id(&pool, Uuid::new_v4())
        .await
        .unwrap();
    assert!(found.is_none());
}

#[tokio::test]
async fn test_get_all_users() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    rustyfin_db::repo_user::create_user(&pool, &make_user("u1", false))
        .await
        .unwrap();
    rustyfin_db::repo_user::create_user(&pool, &make_user("u2", true))
        .await
        .unwrap();

    let users = rustyfin_db::repo_user::get_all_users(&pool).await.unwrap();
    assert_eq!(users.len(), 2);
}

#[tokio::test]
async fn test_delete_user() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let user = make_user("deleteme", false);
    rustyfin_db::repo_user::create_user(&pool, &user)
        .await
        .unwrap();

    let deleted = rustyfin_db::repo_user::delete_user(&pool, user.id)
        .await
        .unwrap();
    assert!(deleted);

    let found = rustyfin_db::repo_user::get_user_by_id(&pool, user.id)
        .await
        .unwrap();
    assert!(found.is_none());

    // Deleting again returns false
    let deleted = rustyfin_db::repo_user::delete_user(&pool, user.id)
        .await
        .unwrap();
    assert!(!deleted);
}

#[tokio::test]
async fn test_update_password() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let user = make_user("pwuser", false);
    rustyfin_db::repo_user::create_user(&pool, &user)
        .await
        .unwrap();

    let updated = rustyfin_db::repo_user::update_password(&pool, user.id, "newhash")
        .await
        .unwrap();
    assert!(updated);

    let found = rustyfin_db::repo_user::get_user_by_id(&pool, user.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.password_hash, "newhash");
}

#[tokio::test]
async fn test_update_policy() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let user = make_user("policyuser", false);
    rustyfin_db::repo_user::create_user(&pool, &user)
        .await
        .unwrap();
    assert!(!user.is_administrator);

    rustyfin_db::repo_user::update_policy(&pool, user.id, true)
        .await
        .unwrap();
    let found = rustyfin_db::repo_user::get_user_by_id(&pool, user.id)
        .await
        .unwrap()
        .unwrap();
    assert!(found.is_administrator);
}

#[tokio::test]
async fn test_update_last_login() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let user = make_user("loginuser", false);
    rustyfin_db::repo_user::create_user(&pool, &user)
        .await
        .unwrap();

    rustyfin_db::repo_user::update_last_login(&pool, user.id)
        .await
        .unwrap();
    let found = rustyfin_db::repo_user::get_user_by_id(&pool, user.id)
        .await
        .unwrap()
        .unwrap();
    assert!(found.last_login_date.is_some());
}

#[tokio::test]
async fn test_update_configuration() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let user = make_user("confuser", false);
    rustyfin_db::repo_user::create_user(&pool, &user)
        .await
        .unwrap();

    let new_config = serde_json::json!({"theme": "dark"});
    rustyfin_db::repo_user::update_configuration(&pool, user.id, &new_config)
        .await
        .unwrap();

    let found = rustyfin_db::repo_user::get_user_by_id(&pool, user.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.configuration["theme"], "dark");
}

// ── Library Repository Tests ──

#[tokio::test]
async fn test_create_and_get_library() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let lib = make_library("Movies", LibraryContentType::Movies);
    rustyfin_db::repo_library::create_library(&pool, &lib)
        .await
        .unwrap();

    let found = rustyfin_db::repo_library::get_library_by_id(&pool, lib.id)
        .await
        .unwrap();
    assert!(found.is_some());
    let found = found.unwrap();
    assert_eq!(found.name, "Movies");
    assert_eq!(found.content_type, LibraryContentType::Movies);
    assert_eq!(found.paths, vec!["/media/movies".to_string()]);
}

#[tokio::test]
async fn test_get_all_libraries() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    rustyfin_db::repo_library::create_library(
        &pool,
        &make_library("Movies", LibraryContentType::Movies),
    )
    .await
    .unwrap();
    rustyfin_db::repo_library::create_library(
        &pool,
        &make_library("TV", LibraryContentType::TvShows),
    )
    .await
    .unwrap();

    let libs = rustyfin_db::repo_library::get_all_libraries(&pool)
        .await
        .unwrap();
    assert_eq!(libs.len(), 2);
}

#[tokio::test]
async fn test_delete_library() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let lib = make_library("ToDelete", LibraryContentType::Mixed);
    rustyfin_db::repo_library::create_library(&pool, &lib)
        .await
        .unwrap();

    rustyfin_db::repo_library::delete_library(&pool, lib.id)
        .await
        .unwrap();
    let found = rustyfin_db::repo_library::get_library_by_id(&pool, lib.id)
        .await
        .unwrap();
    assert!(found.is_none());
}

// ── Session Repository Tests ──

#[tokio::test]
async fn test_upsert_and_get_sessions() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let user = make_user("sessuser", false);
    rustyfin_db::repo_user::create_user(&pool, &user)
        .await
        .unwrap();

    rustyfin_db::repo_session::upsert_session(
        &pool, "sess1", user.id, "dev1", "MyDevice", "TestApp", "1.0",
    )
    .await
    .unwrap();

    let sessions = rustyfin_db::repo_session::get_active_sessions(&pool)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].device_name, "MyDevice");
}

#[tokio::test]
async fn test_delete_session() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let user = make_user("sessuser2", false);
    rustyfin_db::repo_user::create_user(&pool, &user)
        .await
        .unwrap();

    rustyfin_db::repo_session::upsert_session(
        &pool, "sess2", user.id, "dev2", "Device", "App", "1.0",
    )
    .await
    .unwrap();

    rustyfin_db::repo_session::delete_session(&pool, "sess2")
        .await
        .unwrap();
    let sessions = rustyfin_db::repo_session::get_active_sessions(&pool)
        .await
        .unwrap();
    assert!(sessions.is_empty());
}

#[tokio::test]
async fn test_cleanup_expired_sessions() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let user = make_user("expuser", false);
    rustyfin_db::repo_user::create_user(&pool, &user)
        .await
        .unwrap();

    rustyfin_db::repo_session::upsert_session(&pool, "old_sess", user.id, "dev", "D", "A", "1")
        .await
        .unwrap();

    // Manually set last_activity to the past so cleanup finds it
    sqlx::query("UPDATE sessions SET last_activity = datetime('now', '-10 minutes')")
        .execute(&pool)
        .await
        .unwrap();

    let cleaned = rustyfin_db::repo_session::cleanup_expired_sessions(&pool, 5)
        .await
        .unwrap();
    assert!(cleaned >= 1);
}

// ── Device Repository Tests ──

#[tokio::test]
async fn test_upsert_and_get_devices() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let user = make_user("devuser", false);
    rustyfin_db::repo_user::create_user(&pool, &user)
        .await
        .unwrap();

    rustyfin_db::repo_session::upsert_device(
        &pool,
        "device1",
        "Living Room TV",
        "Jellyfin",
        "10.8",
        user.id,
    )
    .await
    .unwrap();

    let devices = rustyfin_db::repo_session::get_all_devices(&pool)
        .await
        .unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name, "Living Room TV");
}

#[tokio::test]
async fn test_delete_device() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let user = make_user("devuser2", false);
    rustyfin_db::repo_user::create_user(&pool, &user)
        .await
        .unwrap();

    rustyfin_db::repo_session::upsert_device(&pool, "device2", "Phone", "App", "1.0", user.id)
        .await
        .unwrap();

    rustyfin_db::repo_session::delete_device(&pool, "device2")
        .await
        .unwrap();
    let devices = rustyfin_db::repo_session::get_all_devices(&pool)
        .await
        .unwrap();
    assert!(devices.is_empty());
}

// ── Media Source & Stream Tests ──

#[tokio::test]
async fn test_insert_media_source_with_streams() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();

    // Create a library and media item
    let lib = make_library("Movies", LibraryContentType::Movies);
    rustyfin_db::repo_library::create_library(&pool, &lib)
        .await
        .unwrap();

    let item_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO media_items (id, library_id, name, media_type, path) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(item_id.to_string())
    .bind(lib.id.to_string())
    .bind("Sample Movie")
    .bind("Movie")
    .bind("/media/movies/sample.mkv")
    .execute(&pool)
    .await
    .unwrap();

    let source = rustyfin_model::media_source::MediaSourceResponse {
        id: Uuid::new_v4().to_string(),
        uid: Uuid::new_v4().to_string(),
        container: Some("mkv".to_string()),
        protocol: "File".to_string(),
        path: "/media/movies/sample.mkv".to_string(),
        r#type: "Default".to_string(),
        run_time_ticks: Some(72000000000),
        size: Some(1024 * 1024 * 500),
        bitrate: Some(2500000),
        video_bitrate: Some(2000000),
        audio_bitrate: Some(500000),
        width: Some(1920),
        height: Some(1080),
        aspect_ratio: Some("16/9".to_string()),
        is_remote: false,
        media_state: None,
        media_streams: vec![
            rustyfin_model::media_source::MediaStream {
                index: 0,
                codec: Some("h264".to_string()),
                codec_type: "Video".to_string(),
                language: Some("eng".to_string()),
                time_base: Some("1/1000".to_string()),
                codec_time_base: None,
                codec_tag: None,
                is_external: false,
                path: None,
                width: Some(1920),
                height: Some(1080),
                average_frame_rate: Some("24".to_string()),
                real_frame_rate: Some("24".to_string()),
                bit_rate: Some(2000000),
                bit_depth: Some(8),
                channels: None,
                channel_layout: None,
                sample_rate: None,
                profile: Some("High".to_string()),
                r#type: None,
                aspect_ratio_text: None,
                aspect_ratio: None,
                title: Some("Main Video".to_string()),
                color_space: None,
                color_transfer: None,
                color_primaries: None,
                color_range: None,
                localized_uri: None,
                is_default: true,
                is_forced: false,
                is_interlaced: false,
                embedded_bitmaps: false,
            },
            rustyfin_model::media_source::MediaStream {
                index: 1,
                codec: Some("aac".to_string()),
                codec_type: "Audio".to_string(),
                language: Some("ger".to_string()),
                time_base: Some("1/1000".to_string()),
                codec_time_base: None,
                codec_tag: None,
                is_external: false,
                path: None,
                width: None,
                height: None,
                average_frame_rate: None,
                real_frame_rate: None,
                bit_rate: Some(500000),
                bit_depth: None,
                channels: Some(6),
                channel_layout: Some("5.1".to_string()),
                sample_rate: Some(48000),
                profile: None,
                r#type: None,
                aspect_ratio_text: None,
                aspect_ratio: None,
                title: Some("German 5.1".to_string()),
                color_space: None,
                color_transfer: None,
                color_primaries: None,
                color_range: None,
                localized_uri: None,
                is_default: true,
                is_forced: false,
                is_interlaced: false,
                embedded_bitmaps: false,
            },
        ],
        supports_transcoding: true,
        supports_direct_stream: true,
        supports_direct_play: true,
    };

    let source_id =
        rustyfin_db::repo_media_source::insert_media_source_with_streams(&pool, &item_id, &source)
            .await
            .unwrap();

    let fetched = rustyfin_db::repo_media_source::get_media_sources(&pool, &item_id)
        .await
        .unwrap();
    assert_eq!(fetched.len(), 1);
    assert_eq!(fetched[0].id, source_id.to_string());
    assert_eq!(fetched[0].media_streams.len(), 2);
    assert_eq!(fetched[0].media_streams[0].codec.as_deref(), Some("h264"));
    assert_eq!(fetched[0].media_streams[1].channels, Some(6));
}

#[tokio::test]
async fn test_get_items_filtered_no_overflow() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();

    let lib = make_library("Movies", LibraryContentType::Movies);
    rustyfin_db::repo_library::create_library(&pool, &lib)
        .await
        .unwrap();

    let item_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO media_items (id, library_id, name, media_type, path) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(item_id.to_string())
    .bind(lib.id.to_string())
    .bind("Sample Movie")
    .bind("Movie")
    .bind("/media/movies/sample.mkv")
    .execute(&pool)
    .await
    .unwrap();

    let (items, count) = rustyfin_db::repo_media::get_items_filtered(
        &pool,
        None,
        Some(lib.id),
        Some(&["Movie".to_string()]),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        0,
        10,
        true,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    assert_eq!(count, 1);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, item_id);
}

#[tokio::test]
async fn test_get_media_sources_batch_and_ordering() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();

    let lib = make_library("Movies", LibraryContentType::Movies);
    rustyfin_db::repo_library::create_library(&pool, &lib)
        .await
        .unwrap();

    let item_id_1 = Uuid::new_v4();
    let item_id_2 = Uuid::new_v4();

    for (item_id, name) in [(item_id_1, "Movie One"), (item_id_2, "Movie Two")] {
        sqlx::query(
            "INSERT INTO media_items (id, library_id, name, media_type, path) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(item_id.to_string())
        .bind(lib.id.to_string())
        .bind(name)
        .bind("Movie")
        .bind(format!("/media/{}.mkv", name))
        .execute(&pool)
        .await
        .unwrap();
    }

    // Source for item 1 with 2 streams
    let source_1 = rustyfin_model::media_source::MediaSourceResponse {
        id: Uuid::new_v4().to_string(),
        uid: "uid-1".to_string(),
        container: Some("mkv".to_string()),
        protocol: "File".to_string(),
        path: "/media/Movie One.mkv".to_string(),
        r#type: "Default".to_string(),
        run_time_ticks: Some(36000000000),
        size: Some(1024000),
        bitrate: Some(2000000),
        video_bitrate: Some(1500000),
        audio_bitrate: Some(500000),
        width: Some(1920),
        height: Some(1080),
        aspect_ratio: Some("16:9".to_string()),
        is_remote: false,
        media_state: None,
        media_streams: vec![
            rustyfin_model::media_source::MediaStream {
                index: 0,
                codec: Some("h264".to_string()),
                codec_type: "Video".to_string(),
                language: None,
                time_base: Some("1/1000".to_string()),
                codec_time_base: None,
                codec_tag: None,
                is_external: false,
                path: None,
                width: Some(1920),
                height: Some(1080),
                average_frame_rate: Some("24".to_string()),
                real_frame_rate: Some("24".to_string()),
                bit_rate: Some(1500000),
                bit_depth: Some(8),
                channels: None,
                channel_layout: None,
                sample_rate: None,
                profile: Some("High".to_string()),
                r#type: None,
                aspect_ratio_text: Some("16:9".to_string()),
                aspect_ratio: Some(1.777),
                title: None,
                color_space: None,
                color_transfer: None,
                color_primaries: None,
                color_range: None,
                localized_uri: None,
                is_default: true,
                is_forced: false,
                is_interlaced: false,
                embedded_bitmaps: false,
            },
            rustyfin_model::media_source::MediaStream {
                index: 1,
                codec: Some("eac3".to_string()),
                codec_type: "Audio".to_string(),
                language: Some("deu".to_string()),
                time_base: Some("1/1000".to_string()),
                codec_time_base: None,
                codec_tag: None,
                is_external: false,
                path: None,
                width: None,
                height: None,
                average_frame_rate: None,
                real_frame_rate: None,
                bit_rate: Some(256000),
                bit_depth: None,
                channels: Some(6),
                channel_layout: Some("5.1".to_string()),
                sample_rate: Some(48000),
                profile: None,
                r#type: None,
                aspect_ratio_text: None,
                aspect_ratio: None,
                title: Some("German 5.1".to_string()),
                color_space: None,
                color_transfer: None,
                color_primaries: None,
                color_range: None,
                localized_uri: None,
                is_default: true,
                is_forced: false,
                is_interlaced: false,
                embedded_bitmaps: false,
            },
        ],
        supports_transcoding: true,
        supports_direct_stream: true,
        supports_direct_play: true,
    };

    // Source for item 2 without streams
    let source_2 = rustyfin_model::media_source::MediaSourceResponse {
        id: Uuid::new_v4().to_string(),
        uid: "uid-2".to_string(),
        container: Some("avi".to_string()),
        protocol: "File".to_string(),
        path: "/media/Movie Two.avi".to_string(),
        r#type: "Default".to_string(),
        run_time_ticks: Some(18000000000),
        size: Some(512000),
        bitrate: Some(1000000),
        video_bitrate: None,
        audio_bitrate: None,
        width: None,
        height: None,
        aspect_ratio: None,
        is_remote: false,
        media_state: None,
        media_streams: vec![],
        supports_transcoding: true,
        supports_direct_stream: true,
        supports_direct_play: true,
    };

    rustyfin_db::repo_media_source::insert_media_source_with_streams(&pool, &item_id_1, &source_1)
        .await
        .unwrap();

    rustyfin_db::repo_media_source::insert_media_source_with_streams(&pool, &item_id_2, &source_2)
        .await
        .unwrap();

    let batch =
        rustyfin_db::repo_media_source::get_media_sources_batch(&pool, &[item_id_1, item_id_2])
            .await
            .unwrap();

    assert_eq!(batch.len(), 2);
    let sources_1 = batch.get(&item_id_1.to_string()).unwrap();
    assert_eq!(sources_1.len(), 1);
    assert_eq!(sources_1[0].media_streams.len(), 2);
    assert_eq!(sources_1[0].media_streams[0].index, 0);
    assert_eq!(sources_1[0].media_streams[1].index, 1);
    assert_eq!(sources_1[0].media_streams[0].codec.as_deref(), Some("h264"));
    assert_eq!(sources_1[0].media_streams[1].codec.as_deref(), Some("eac3"));

    let sources_2 = batch.get(&item_id_2.to_string()).unwrap();
    assert_eq!(sources_2.len(), 1);
    assert_eq!(sources_2[0].media_streams.len(), 0);
}

#[tokio::test]
async fn test_get_media_sources_batch_large() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let lib = make_library("LargeLib", LibraryContentType::Movies);
    rustyfin_db::repo_library::create_library(&pool, &lib)
        .await
        .unwrap();

    // Create 450 items (exceeding chunk size of 200)
    let mut item_ids = Vec::new();
    for i in 0..450 {
        let item_id = Uuid::new_v4();
        let path = format!("/media/movie_{}.mkv", i);
        sqlx::query(
            "INSERT INTO media_items (id, library_id, name, media_type, path) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(item_id.to_string())
        .bind(lib.id.to_string())
        .bind(format!("Movie {}", i))
        .bind("Movie")
        .bind(&path)
        .execute(&pool)
        .await
        .unwrap();

        item_ids.push(item_id);

        if i % 50 == 0 {
            let source = rustyfin_model::media_source::MediaSourceResponse {
                id: Uuid::new_v4().to_string(),
                uid: format!("uid-{}", i),
                container: Some("mkv".to_string()),
                protocol: "File".to_string(),
                path: path.clone(),
                r#type: "Default".to_string(),
                run_time_ticks: Some(36000000000),
                size: Some(1024000),
                bitrate: Some(2000000),
                video_bitrate: None,
                audio_bitrate: None,
                width: None,
                height: None,
                aspect_ratio: None,
                is_remote: false,
                media_state: None,
                media_streams: vec![],
                supports_transcoding: true,
                supports_direct_stream: true,
                supports_direct_play: true,
            };
            rustyfin_db::repo_media_source::insert_media_source_with_streams(
                &pool, &item_id, &source,
            )
            .await
            .unwrap();
        }
    }

    let batch = rustyfin_db::repo_media_source::get_media_sources_batch(&pool, &item_ids)
        .await
        .unwrap();

    // 450 items total, 9 of them (0, 50, 100, 150, 200, 250, 300, 350, 400) have sources
    assert_eq!(batch.len(), 9);
    for i in (0..450).step_by(50) {
        let id_str = item_ids[i].to_string();
        assert!(batch.contains_key(&id_str));
    }
}

#[tokio::test]
async fn test_filtered_query_with_genres_and_tags() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let lib = make_library("FilterLib", LibraryContentType::Movies);
    rustyfin_db::repo_library::create_library(&pool, &lib)
        .await
        .unwrap();

    let id1 = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO media_items (id, library_id, name, media_type, path, genres) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(id1.to_string())
    .bind(lib.id.to_string())
    .bind("Sci-Fi Thriller")
    .bind("Movie")
    .bind("/media/scifi.mkv")
    .bind("Sci-Fi|Thriller")
    .execute(&pool)
    .await
    .unwrap();

    let id2 = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO media_items (id, library_id, name, media_type, path, genres) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(id2.to_string())
    .bind(lib.id.to_string())
    .bind("Action Comedy")
    .bind("Movie")
    .bind("/media/action.mkv")
    .bind("Action|Comedy")
    .execute(&pool)
    .await
    .unwrap();

    // Test genre filtering
    let sci_fi_genre = vec!["Sci-Fi".to_string()];
    let (items, total) = rustyfin_db::repo_media::get_items_filtered(
        &pool,
        None,
        Some(lib.id),
        None,
        None,
        Some(&sci_fi_genre),
        None,
        None,
        None,
        None,
        None,
        None,
        0,
        10,
        false,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    assert_eq!(total, 1);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "Sci-Fi Thriller");
}

#[tokio::test]
async fn test_filtered_query_with_name_starts_with() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let lib = make_library("AlphaLib", LibraryContentType::Movies);
    rustyfin_db::repo_library::create_library(&pool, &lib)
        .await
        .unwrap();

    let items_data = [
        (Uuid::new_v4(), "Alien", "Alien"),
        (Uuid::new_v4(), "Avatar", "Avatar"),
        (Uuid::new_v4(), "Blade Runner", "Blade Runner"),
        (Uuid::new_v4(), "The Matrix", "Matrix, The"),
        (
            Uuid::new_v4(),
            "2001: A Space Odyssey",
            "2001: A Space Odyssey",
        ),
    ];

    for (id, name, sort_name) in items_data {
        sqlx::query(
            "INSERT INTO media_items (id, library_id, name, sort_name, media_type, path) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(lib.id.to_string())
        .bind(name)
        .bind(sort_name)
        .bind("Movie")
        .bind(format!("/media/{}.mkv", name))
        .execute(&pool)
        .await
        .unwrap();
    }

    // Filter by 'A'
    let (items_a, total_a) = rustyfin_db::repo_media::get_items_filtered(
        &pool,
        None,
        Some(lib.id),
        None,
        None,
        None,
        None,
        None,
        Some("A"),
        None,
        None,
        None,
        0,
        100,
        false,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    assert_eq!(total_a, 2);
    assert_eq!(items_a.len(), 2);

    // Filter by 'M' (matches "The Matrix" via sort_name)
    let (items_m, total_m) = rustyfin_db::repo_media::get_items_filtered(
        &pool,
        None,
        Some(lib.id),
        None,
        None,
        None,
        None,
        None,
        Some("M"),
        None,
        None,
        None,
        0,
        100,
        false,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    assert_eq!(total_m, 1);
    assert_eq!(items_m[0].name, "The Matrix");

    // Filter by '#' (matches numbers / symbols like "2001: A Space Odyssey")
    let (items_hash, total_hash) = rustyfin_db::repo_media::get_items_filtered(
        &pool,
        None,
        Some(lib.id),
        None,
        None,
        None,
        None,
        None,
        Some("#"),
        None,
        None,
        None,
        0,
        100,
        false,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    assert_eq!(total_hash, 1);
    assert_eq!(items_hash[0].name, "2001: A Space Odyssey");
}

// ── Media Item Repository Tests ──

fn make_movie(library_id: Uuid, name: &str) -> MediaItem {
    MediaItem {
        id: Uuid::new_v4(),
        name: name.to_string(),
        sort_name: Some(name.to_lowercase()),
        media_type: MediaType::Movie,
        path: format!("/media/movies/{name}.mkv"),
        library_id,
        production_year: Some(2020),
        popularity: Some(12.5),
        ..MediaItem::default_dto_fields()
    }
}

#[tokio::test]
async fn test_upsert_media_item_round_trip() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();

    let lib = make_library("Movies", LibraryContentType::Movies);
    rustyfin_db::repo_library::create_library(&pool, &lib)
        .await
        .unwrap();

    let movie = make_movie(lib.id, "Test Movie");
    rustyfin_db::repo_media::upsert_media_item(&pool, &movie)
        .await
        .unwrap();

    let stored = rustyfin_db::repo_media::get_item_by_id(&pool, movie.id)
        .await
        .unwrap()
        .expect("item should exist after upsert");
    assert_eq!(stored.name, "Test Movie");
    assert_eq!(stored.path, movie.path);
    assert_eq!(stored.production_year, Some(2020));
    assert_eq!(stored.popularity, Some(12.5));
}

#[tokio::test]
async fn test_batch_upsert_media_items_round_trip() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();

    let lib = make_library("Movies", LibraryContentType::Movies);
    rustyfin_db::repo_library::create_library(&pool, &lib)
        .await
        .unwrap();

    let movies = vec![make_movie(lib.id, "First"), make_movie(lib.id, "Second")];
    rustyfin_db::repo_media::batch_upsert_media_items(&pool, &movies)
        .await
        .unwrap();

    for movie in &movies {
        let stored = rustyfin_db::repo_media::get_item_by_id(&pool, movie.id)
            .await
            .unwrap()
            .expect("item should exist after batch upsert");
        assert_eq!(stored.name, movie.name);
        assert_eq!(stored.popularity, Some(12.5));
    }
}

/// A batch that saves a stored item again updates it in place (B31): the rows that reference it
/// and its date added stay, and the year and rating counts follow its new values.
#[tokio::test]
async fn test_batch_upsert_updates_a_stored_item_in_place() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();

    let lib = make_library("Movies", LibraryContentType::Movies);
    rustyfin_db::repo_library::create_library(&pool, &lib)
        .await
        .unwrap();
    let added: chrono::DateTime<Utc> = "2020-01-02T03:04:05Z".parse().unwrap();
    let movie = MediaItem {
        official_rating: Some("R".to_string()),
        date_created: added,
        ..make_movie(lib.id, "Stored")
    };
    let other = MediaItem {
        official_rating: Some("R".to_string()),
        ..make_movie(lib.id, "Other")
    };
    rustyfin_db::repo_media::batch_upsert_media_items(&pool, &[movie.clone(), other])
        .await
        .unwrap();
    sqlx::query("INSERT INTO item_images (item_id, image_type, path) VALUES (?1, 'Primary', ?2)")
        .bind(movie.id.to_string())
        .bind("/media/movies/Stored.jpg")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO media_sources (id, item_id, uid, path) VALUES (?1, ?2, ?1, ?3)")
        .bind(Uuid::new_v4().to_string())
        .bind(movie.id.to_string())
        .bind(&movie.path)
        .execute(&pool)
        .await
        .unwrap();

    let changed = MediaItem {
        name: "Renamed".to_string(),
        production_year: Some(2021),
        official_rating: Some("PG".to_string()),
        date_created: Utc::now(),
        ..movie.clone()
    };
    rustyfin_db::repo_media::batch_upsert_media_items(&pool, &[changed])
        .await
        .unwrap();

    let stored = rustyfin_db::repo_media::get_item_by_id(&pool, movie.id)
        .await
        .unwrap()
        .expect("the item is still stored under its id");
    assert_eq!(stored.name, "Renamed");
    let kept: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM item_images WHERE item_id = ?1), \
                (SELECT COUNT(*) FROM media_sources WHERE item_id = ?1)",
    )
    .bind(movie.id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kept, (1, 1), "the image and the media source stay");
    assert_eq!(stored.date_created, added);
    let years: Vec<(i64, i64)> =
        sqlx::query_as("SELECT year, item_count FROM filter_year_index ORDER BY year")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(years, [(2020, 1), (2021, 1)]);
    let ratings: Vec<(String, i64)> =
        sqlx::query_as("SELECT rating, item_count FROM filter_rating_index ORDER BY rating")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(ratings, [("PG".to_string(), 1), ("R".to_string(), 1)]);
}

/// The year filter's values with their item counts.
async fn filter_years(pool: &SqlitePool) -> Vec<(i64, i64)> {
    sqlx::query_as("SELECT year, item_count FROM filter_year_index ORDER BY year")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// The rating filter's values with their item counts.
async fn filter_ratings(pool: &SqlitePool) -> Vec<(String, i64)> {
    sqlx::query_as("SELECT rating, item_count FROM filter_rating_index ORDER BY rating")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Stores a 2020, R movie and a 2021, PG one, and returns the first.
async fn store_two_rated_movies(pool: &SqlitePool) -> MediaItem {
    let lib = make_library("Movies", LibraryContentType::Movies);
    rustyfin_db::repo_library::create_library(pool, &lib)
        .await
        .unwrap();
    let movie = MediaItem {
        official_rating: Some("R".to_string()),
        ..make_movie(lib.id, "Last")
    };
    let other = MediaItem {
        production_year: Some(2021),
        official_rating: Some("PG".to_string()),
        ..make_movie(lib.id, "Other")
    };
    for item in [&movie, &other] {
        rustyfin_db::repo_media::upsert_media_item(pool, item)
            .await
            .unwrap();
    }
    assert_eq!(filter_years(pool).await, [(2020, 1), (2021, 1)]);
    assert_eq!(
        filter_ratings(pool).await,
        [("PG".to_string(), 1), ("R".to_string(), 1)]
    );
    movie
}

/// B31 review 1, finding 4: a year or rating leaves the filters once no item has it. Here the only
/// 2020, R movie is saved as 2022, PG-13.
#[tokio::test]
async fn test_filters_drop_a_value_when_its_last_item_moves_off_it() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();

    let movie = store_two_rated_movies(&pool).await;
    let moved = MediaItem {
        production_year: Some(2022),
        official_rating: Some("PG-13".to_string()),
        ..movie
    };
    rustyfin_db::repo_media::upsert_media_item(&pool, &moved)
        .await
        .unwrap();

    assert_eq!(
        (filter_years(&pool).await, filter_ratings(&pool).await),
        (
            vec![(2021, 1), (2022, 1)],
            vec![("PG".to_string(), 1), ("PG-13".to_string(), 1)]
        ),
        "no item has 2020 or R any more"
    );
}

/// B31 review 1, finding 4: the same when the only 2020, R movie is deleted.
#[tokio::test]
async fn test_filters_drop_a_value_when_its_last_item_is_deleted() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();

    let movie = store_two_rated_movies(&pool).await;
    assert!(
        rustyfin_db::repo_media::delete_item(&pool, movie.id)
            .await
            .unwrap()
    );

    assert_eq!(
        (filter_years(&pool).await, filter_ratings(&pool).await),
        (vec![(2021, 1)], vec![("PG".to_string(), 1)]),
        "no item has 2020 or R any more"
    );
}

/// B26 review 1, finding 2: `save_chapters` saves all of an item's chapters or none of them. A save
/// that fails part way, here at a second chapter with the first one's id, leaves the chapters that
/// the item had before.
#[tokio::test]
async fn test_save_chapters_keeps_the_old_ones_when_it_fails() {
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let lib = make_library("Movies", LibraryContentType::Movies);
    rustyfin_db::repo_library::create_library(&pool, &lib)
        .await
        .unwrap();
    let item_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO media_items (id, library_id, name, media_type, path) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(item_id.to_string())
    .bind(lib.id.to_string())
    .bind("Sample Movie")
    .bind("Movie")
    .bind("/media/movies/sample.mkv")
    .execute(&pool)
    .await
    .unwrap();

    let chapter = |id: &str, number: i32| rustyfin_model::chapter::Chapter {
        id: id.to_string(),
        item_id: item_id.to_string(),
        chapter_number: number,
        name: format!("Chapter {number}"),
        start_position_ticks: i64::from(number - 1) * 10_000_000,
        end_position_ticks: i64::from(number) * 10_000_000,
        image_path: None,
    };
    rustyfin_db::repo_chapter::save_chapters(
        &pool,
        item_id,
        vec![chapter("c1", 1), chapter("c2", 2)],
    )
    .await
    .unwrap();

    let failed = rustyfin_db::repo_chapter::save_chapters(
        &pool,
        item_id,
        vec![chapter("c3", 1), chapter("c3", 2)],
    )
    .await;
    assert!(
        failed.is_err(),
        "two chapters with one id were saved: {failed:?}"
    );
    let kept: Vec<(String, String)> = rustyfin_db::repo_chapter::get_chapters(&pool, item_id)
        .await
        .unwrap()
        .into_iter()
        .map(|chapter| (chapter.id, chapter.name))
        .collect();
    assert_eq!(
        kept,
        [
            ("c1".to_string(), "Chapter 1".to_string()),
            ("c2".to_string(), "Chapter 2".to_string())
        ],
        "the failed save left part of its chapters"
    );
}

// ── B34: the filter trigger's rating recounts use an index ──

const B34_ITEMS: i64 = 20_000;
const B34_RATINGS: [&str; 8] = ["G", "PG", "PG-13", "R", "NC-17", "TV-14", "TV-MA", "NR"];

/// The details of `EXPLAIN QUERY PLAN <sql>` with `R` bound to `?1`.
async fn b34_plan(pool: &SqlitePool, sql: &'static str) -> Vec<String> {
    let rows: Vec<(i64, i64, i64, String)> =
        sqlx::query_as(sql).bind("R").fetch_all(pool).await.unwrap();
    rows.into_iter().map(|(_, _, _, detail)| detail).collect()
}

/// Stores `B34_ITEMS` movies with no year or rating, as a first import finds them. A helper index
/// keeps the insert trigger's recounts from scanning the table while they are stored, whatever the
/// migrations made; it is dropped again before anything is timed. Returns their ids.
async fn b34_items(pool: &SqlitePool) -> Vec<String> {
    let library = make_library("Film", LibraryContentType::Movies);
    rustyfin_db::repo_library::create_library(pool, &library)
        .await
        .unwrap();
    sqlx::query("CREATE INDEX b34_insert_helper ON media_items(official_rating)")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i + 1 < ?1) \
         INSERT INTO media_items \
         (id, name, sort_name, media_type, path, library_id, genres, production_countries) \
         SELECT printf('b34-%05d', i), printf('Film %05d', i), printf('film %05d', i), 'Movie', \
          printf('/media/film/%05d.mkv', i), ?2, '', '' FROM n",
    )
    .bind(B34_ITEMS)
    .bind(library.id.to_string())
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("DROP INDEX b34_insert_helper")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query_scalar("SELECT id FROM media_items ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Writes every item once with the Jellyfin import's UPDATE (`repo_jellyfin_import.rs`,
/// `write_metadata`) in one transaction, giving each a year and a rating. Stops once `limit` has
/// passed. Returns how many items it wrote and how long it took.
async fn b34_import_update(
    pool: &SqlitePool,
    ids: &[String],
    limit: std::time::Duration,
) -> (usize, std::time::Duration) {
    let start = std::time::Instant::now();
    let mut tx = pool.begin().await.unwrap();
    let mut written = 0;
    for (i, id) in ids.iter().enumerate() {
        if start.elapsed() > limit {
            break;
        }
        sqlx::query(
            "UPDATE media_items SET name = ?, sort_name = ?, overview = ?, tagline = ?, \
             official_rating = ?, community_rating = ?, premiere_date = ?, end_date = ?, \
             production_year = ?, genres = ?, tags = ?, tmdb_id = ?, tvdb_id = ? WHERE id = ?",
        )
        .bind(format!("Film {i:05}"))
        .bind(format!("film {i:05}"))
        .bind("Imported overview")
        .bind(None::<String>)
        .bind(B34_RATINGS[i % B34_RATINGS.len()])
        .bind(7.0_f64)
        .bind(None::<String>)
        .bind(None::<String>)
        .bind(1950 + (i % 70) as i64)
        .bind("Drama")
        .bind("")
        .bind(None::<i64>)
        .bind(None::<i64>)
        .bind(id)
        .execute(&mut *tx)
        .await
        .unwrap();
        written += 1;
    }
    tx.commit().await.unwrap();
    (written, start.elapsed())
}

/// The update trigger recounts the old and new rating of every item whose rating is written,
/// with these two statements. Both look the rating up in its index instead of scanning the table.
#[tokio::test]
async fn b34_rating_recounts_use_the_index() {
    let db = test_pool().await;
    for sql in [
        "EXPLAIN QUERY PLAN \
         SELECT ?1, COUNT(*) FROM media_items WHERE official_rating = ?1 GROUP BY ?1",
        "EXPLAIN QUERY PLAN SELECT 1 FROM media_items WHERE official_rating = ?1",
    ] {
        let plan = b34_plan(&db.pool, sql).await;
        assert!(
            plan.iter()
                .any(|step| step.contains("idx_media_items_official_rating")),
            "{sql}: {plan:?}"
        );
    }
}

/// An import that writes 20,000 items in one transaction holds the write lock for well under the
/// 30 s busy timeout other writers wait. Without the index each item's recounts scanned the table
/// twice, about 35-43 s in all (B20's re-check 9). It times the debug build's SQLite, and its time
/// grows several times over on a loaded machine, so like the timing without the index below it only
/// runs when asked; `b34_rating_recounts_use_the_index` guards the index on every run.
#[tokio::test]
#[ignore]
async fn b34_import_update_of_20000_items_is_quick() {
    let db = test_pool().await;
    let ids = b34_items(&db.pool).await;
    let limit = std::time::Duration::from_secs(15);

    let (written, took) = b34_import_update(&db.pool, &ids, limit).await;

    eprintln!("b34: import UPDATE of {written} items took {took:?}");
    assert_eq!(written, ids.len(), "stopped after {limit:?}");
    let ratings: i64 = sqlx::query_scalar("SELECT SUM(item_count) FROM filter_rating_index")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(ratings, B34_ITEMS, "every item counted under its rating");
}

/// The same import with the rating index dropped, for the before timing. It takes minutes. Run both
/// timings with `cargo test -p rustyfin-db --test repo_tests b34_import -- --ignored --nocapture`.
#[tokio::test]
#[ignore]
async fn b34_import_update_of_20000_items_without_the_index() {
    let db = test_pool().await;
    let ids = b34_items(&db.pool).await;
    sqlx::query("DROP INDEX IF EXISTS idx_media_items_official_rating")
        .execute(&db.pool)
        .await
        .unwrap();

    let (written, took) =
        b34_import_update(&db.pool, &ids, std::time::Duration::from_secs(1800)).await;

    eprintln!("b34: import UPDATE of {written} items without the index took {took:?}");
}

// ── B34: refresh saves under the scan's concurrency (F9) ──

const B34_SCAN_ITEMS: usize = 400;

/// What one of the scan's refresh tasks does to the database for a new item (metadata.rs): it
/// reads the item, stores its local poster, then saves the fields its NFO filled. A failure names
/// its step.
async fn b34_refresh_like_the_scan(pool: &SqlitePool, id: Uuid, n: usize) -> Result<(), String> {
    use rustyfin_model::image::{ImageType, ItemImage};
    let read = rustyfin_db::repo_media::get_item_by_id(pool, id)
        .await
        .map_err(|e| format!("read: {e}"))?
        .expect("stored");
    let poster = ItemImage {
        item_id: id,
        image_type: ImageType::Primary,
        image_index: 0,
        path: format!("/media/movies/Film {n}/poster.jpg"),
        provider: "local".to_string(),
        width: None,
        height: None,
        blurhash: None,
    };
    rustyfin_db::repo_image::batch_upsert_images(pool, &[poster])
        .await
        .map_err(|e| format!("images: {e}"))?;
    let mut updated = read.clone();
    updated.overview = Some(format!("Overview {n}"));
    updated.genres = vec!["Drama".to_string(), format!("Genre {}", n % 7)];
    updated.official_rating = Some(B34_RATINGS[n % B34_RATINGS.len()].to_string());
    updated.production_year = Some(1990 + (n % 30) as i32);
    updated.taglines = vec![format!("Tagline {n}")];
    rustyfin_db::repo_media::save_refresh_changes(pool, &read, &updated)
        .await
        .map_err(|e| format!("save: {e}"))?;
    Ok(())
}

/// The scan refreshes its new items ten at a time (scanner.rs) on the shared-cache pool while
/// clients search. A lock conflict there is SQLITE_LOCKED, which the busy timeout never waits for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn b34_concurrent_refresh_saves_hit_no_lock_errors() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let db = test_pool().await;
    let lib = make_library("Movies", LibraryContentType::Movies);
    rustyfin_db::repo_library::create_library(&db.pool, &lib)
        .await
        .unwrap();
    let items: Vec<MediaItem> = (0..B34_SCAN_ITEMS)
        .map(|n| make_movie(lib.id, &format!("Film {n}")))
        .collect();
    rustyfin_db::repo_media::batch_upsert_media_items(&db.pool, &items)
        .await
        .unwrap();

    let done = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..2)
        .map(|_| {
            let (pool, done) = (db.pool.clone(), done.clone());
            tokio::spawn(async move {
                let mut failures = Vec::new();
                while !done.load(Ordering::Relaxed) {
                    let found = rustyfin_db::repo_search::search_items(&pool, "Film", &[], 20);
                    if let Err(e) = found.await {
                        failures.push(format!("search: {e}"));
                    }
                }
                failures
            })
        })
        .collect();

    let started = std::time::Instant::now();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(10));
    let tasks: Vec<_> = items
        .iter()
        .enumerate()
        .map(|(n, item)| {
            let (pool, semaphore, id) = (db.pool.clone(), semaphore.clone(), item.id);
            tokio::spawn(async move {
                let _permit = semaphore.acquire_owned().await.unwrap();
                b34_refresh_like_the_scan(&pool, id, n).await.err()
            })
        })
        .collect();
    // A lock cycle the deadlock check misses leaves every connection waiting for good.
    let joined = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let mut failures = Vec::new();
        for task in tasks {
            failures.extend(task.await.unwrap());
        }
        failures
    });
    let failures = joined.await.expect("the refreshes finish within 60 s");
    done.store(true, Ordering::Relaxed);
    let mut reader_failures = Vec::new();
    for reader in readers {
        reader_failures.extend(reader.await.unwrap());
    }

    let saves = failures.iter().filter(|e| e.starts_with("save:")).count();
    eprintln!(
        "b34: {B34_SCAN_ITEMS} refreshes in {:?}: {saves} saves, {} other steps and {} searches failed; first: {:?}",
        started.elapsed(),
        failures.len() - saves,
        reader_failures.len(),
        failures.first().or(reader_failures.first())
    );
    assert!(failures.is_empty(), "refresh steps failed: {failures:?}");
    let saved: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM media_items WHERE overview LIKE 'Overview %'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(saved, B34_SCAN_ITEMS as i64, "every refresh saved");
    sqlx::query("INSERT INTO media_items_fts(media_items_fts, rank) VALUES('integrity-check', 1)")
        .execute(&db.pool)
        .await
        .expect("the search index matches the items");
}

// ── B20: one-time setup codes ──

async fn b20_password(pool: &SqlitePool, user: Uuid) -> String {
    rustyfin_db::repo_user::get_user_by_id(pool, user)
        .await
        .unwrap()
        .unwrap()
        .password_hash
}

#[tokio::test]
async fn b20_redeem_needs_the_current_code() {
    use rustyfin_db::repo_setup_code::{code_hash, create_user_with_code, redeem, replace_code};
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let user = make_user("pending", false);
    create_user_with_code(&pool, &user, "hash-1").await.unwrap();
    assert_eq!(
        code_hash(&pool, user.id).await.unwrap().as_deref(),
        Some("hash-1")
    );
    assert_eq!(code_hash(&pool, Uuid::new_v4()).await.unwrap(), None);

    // A reissue replaces the hash; the old one then redeems nothing.
    assert!(
        replace_code(&pool, user.id, "hash-2", Utc::now())
            .await
            .unwrap()
    );
    assert!(!redeem(&pool, user.id, "hash-1", "pw-1").await.unwrap());
    assert_eq!(b20_password(&pool, user.id).await, user.password_hash);
    assert_eq!(
        code_hash(&pool, user.id).await.unwrap().as_deref(),
        Some("hash-2")
    );

    // The current hash sets the password and removes the code, once.
    assert!(redeem(&pool, user.id, "hash-2", "pw-2").await.unwrap());
    assert_eq!(b20_password(&pool, user.id).await, "pw-2");
    assert_eq!(code_hash(&pool, user.id).await.unwrap(), None);
    assert!(!redeem(&pool, user.id, "hash-2", "pw-3").await.unwrap());
    assert_eq!(b20_password(&pool, user.id).await, "pw-2");
    assert!(
        !replace_code(&pool, user.id, "hash-3", Utc::now())
            .await
            .unwrap()
    );
    assert_eq!(code_hash(&pool, user.id).await.unwrap(), None);
}

#[tokio::test]
async fn b20_pending_users_are_sorted_by_name() {
    use rustyfin_db::repo_setup_code::{PendingUser, create_user_with_code, pending_users};
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    assert!(pending_users(&pool).await.unwrap().is_empty());
    let carol = make_user("Carol", false);
    let bob = make_user("bob", false);
    let alice = make_user("alice", false);
    for user in [&carol, &bob, &alice] {
        create_user_with_code(&pool, user, "hash").await.unwrap();
    }
    let dave = make_user("dave", false);
    rustyfin_db::repo_user::create_user(&pool, &dave)
        .await
        .unwrap();

    let pending = |user: &User| PendingUser {
        user_id: user.id,
        name: user.name.clone(),
        created_at: user.created_at,
    };
    let expected = vec![pending(&alice), pending(&bob), pending(&carol)];
    assert_eq!(pending_users(&pool).await.unwrap(), expected);

    let later = Utc::now() + chrono::Duration::seconds(5);
    let replaced = rustyfin_db::repo_setup_code::replace_code(&pool, bob.id, "hash-2", later);
    assert!(replaced.await.unwrap());
    let listed = pending_users(&pool).await.unwrap();
    assert_eq!(listed[1].user_id, bob.id);
    assert_eq!(listed[1].created_at, later);
}

#[tokio::test]
async fn b20_password_writes_end_the_code() {
    use rustyfin_db::repo_setup_code::{code_hash, create_user_with_code};
    let temp_db = test_pool().await;
    let pool = temp_db.pool.clone();
    let bob = make_user("bob", false);
    let carol = make_user("carol", false);
    for user in [&bob, &carol] {
        create_user_with_code(&pool, user, "hash").await.unwrap();
    }
    let updated = rustyfin_db::repo_user::update_password(&pool, bob.id, "newhash");
    assert!(updated.await.unwrap());
    assert_eq!(b20_password(&pool, bob.id).await, "newhash");
    assert_eq!(code_hash(&pool, bob.id).await.unwrap(), None);
    assert_eq!(
        code_hash(&pool, carol.id).await.unwrap().as_deref(),
        Some("hash")
    );
    let unknown = rustyfin_db::repo_user::update_password(&pool, Uuid::new_v4(), "x");
    assert!(!unknown.await.unwrap());
    assert_eq!(
        code_hash(&pool, carol.id).await.unwrap().as_deref(),
        Some("hash")
    );
}
