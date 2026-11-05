//! Library scans of real files in a temp dir: ignored folders and files (R1-12), symlink
//! loops (M-19), removal of items that a scan no longer collects, and episode numbers (R1-13).

use std::path::{Path, PathBuf};

use rustyfin_core::scanner;
use rustyfin_model::library::{Library, LibraryContentType};
use sqlx::SqlitePool;
use uuid::Uuid;

/// A temp dir holding a database and the test media; removed on drop.
struct Fixture {
    pool: SqlitePool,
    dir: PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("rustyfin_test_scan_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("test.db");
        let pool = rustyfin_db::init_db(db.to_str().unwrap()).await.unwrap();
        Self { pool, dir }
    }

    /// `<temp dir>/<rel>`.
    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    /// Creates a library whose one root is `<temp dir>/<root>`.
    async fn library(&self, root: &str, content_type: LibraryContentType) -> Library {
        let root = self.path(root);
        std::fs::create_dir_all(&root).unwrap();
        let lib = Library {
            id: Uuid::new_v4(),
            name: root.file_name().unwrap().to_str().unwrap().to_string(),
            content_type,
            paths: vec![root.to_str().unwrap().to_string()],
        };
        rustyfin_db::repo_library::create_library(&self.pool, &lib)
            .await
            .unwrap();
        lib
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn root_of(lib: &Library) -> PathBuf {
    PathBuf::from(&lib.paths[0])
}

/// Creates an empty file at `<root>/<rel>`, with its parent folders.
fn touch(root: &Path, rel: &str) -> PathBuf {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"").unwrap();
    path
}

async fn scan(pool: &SqlitePool, lib: &Library) -> u32 {
    scanner::scan_library(pool, lib)
        .await
        .unwrap_or_else(|e| panic!("scan of {}: {e}", lib.name))
}

/// The paths of the library's items of `media_type`, relative to its root, sorted.
async fn paths(pool: &SqlitePool, lib: &Library, media_type: &str) -> Vec<String> {
    let root = root_of(lib);
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT path FROM media_items WHERE library_id = ?1 AND media_type = ?2",
    )
    .bind(lib.id.to_string())
    .bind(media_type)
    .fetch_all(pool)
    .await
    .unwrap();
    let mut rel: Vec<String> = rows
        .into_iter()
        .map(|p| match Path::new(&p).strip_prefix(&root) {
            Ok(r) => r.to_str().unwrap().to_string(),
            Err(_) => p,
        })
        .collect();
    rel.sort();
    rel
}

/// Runs `sql` with `binds` and panics on error.
async fn exec(pool: &SqlitePool, sql: &'static str, binds: &[&str]) {
    let mut query = sqlx::query(sql);
    for b in binds {
        query = query.bind(b.to_string());
    }
    query
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

async fn count(pool: &SqlitePool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

/// Inserts an item the way the old scanner stored it, and returns its id.
async fn insert_item(
    pool: &SqlitePool,
    lib: &Library,
    media_type: &str,
    path: &Path,
    parent: Option<Uuid>,
) -> Uuid {
    let id = Uuid::new_v4();
    let name = path.file_stem().unwrap().to_str().unwrap();
    sqlx::query(
        "INSERT INTO media_items (id, name, sort_name, media_type, path, library_id, parent_id)
         VALUES (?1, ?2, lower(?2), ?3, ?4, ?5, ?6)",
    )
    .bind(id.to_string())
    .bind(name)
    .bind(media_type)
    .bind(path.to_str().unwrap())
    .bind(lib.id.to_string())
    .bind(parent.map(|p| p.to_string()))
    .execute(pool)
    .await
    .unwrap();
    id
}

/// One episode: its path relative to the root, `index_number`, `parent_index_number`, and the
/// name of its season item.
type EpisodeRow = (String, Option<i64>, Option<i64>, String);

/// The library's episodes, sorted by path.
async fn episodes(pool: &SqlitePool, lib: &Library) -> Vec<EpisodeRow> {
    let root = root_of(lib);
    let rows: Vec<EpisodeRow> = sqlx::query_as(
        "SELECT e.path, e.index_number, e.parent_index_number, s.name
         FROM media_items AS e JOIN media_items AS s ON s.id = e.parent_id
         WHERE e.library_id = ?1 AND e.media_type = 'Episode'",
    )
    .bind(lib.id.to_string())
    .fetch_all(pool)
    .await
    .unwrap();
    let mut rows: Vec<EpisodeRow> = rows
        .into_iter()
        .map(|(path, index, season, season_name)| {
            let rel = Path::new(&path).strip_prefix(&root).unwrap();
            (
                rel.to_str().unwrap().to_string(),
                index,
                season,
                season_name,
            )
        })
        .collect();
    rows.sort();
    rows
}

async fn insert_user(pool: &SqlitePool) -> String {
    let id = Uuid::new_v4().to_string();
    let name = format!("user-{id}");
    exec(
        pool,
        "INSERT INTO users (id, name, password_hash) VALUES (?1, ?2, 'x')",
        &[&id, &name],
    )
    .await;
    id
}

#[tokio::test]
async fn scan_skips_ignored_dirs_and_files() {
    let fx = Fixture::new().await;
    let film = fx.library("film", LibraryContentType::Movies).await;
    let root = root_of(&film);
    for rel in [
        "Argo (2012)/Argo (2012).mkv",
        "Argo (2012)/.@__thumb/Argo (2012).mkv",
        "Argo (2012)/@eaDir/Argo (2012).mkv",
        "Argo (2012)/._Argo (2012).mkv",
        "Argo (2012)/Argo (2012).sample.mkv",
        "Argo (2012)/sample.mkv",
        "Argo (2012)/Sample/Argo (2012).mkv",
        "Argo (2012)/metadata/Argo (2012).mkv",
        "Argo (2012)/Argo (2012).trickplay/Argo (2012).mkv",
        "Amadeus (1984)/Amadeus (1984).mkv",
        "Amadeus (1984)/Featurettes/Making Of.mkv",
        "Sample Movie (2010)/Sample Movie (2010).mkv",
        "#recycle/Old (2001)/Old (2001).mkv",
        "@Recycle/Old (2002).mkv",
        "lost+found/Lost (2003).mkv",
        ".hidden/Hidden (2004).mkv",
        "$RECYCLE.BIN/Bin (2005).mkv",
        "System Volume Information/Svi (2006).mkv",
    ] {
        touch(&root, rel);
    }
    let tv = fx.library("tv", LibraryContentType::TvShows).await;
    let tv_root = root_of(&tv);
    for rel in [
        "Show/Season 1/Show - S01E01.mkv",
        "Show/Season 1/.@__thumb/Show - S01E01.mkv",
        "Show/Season 1/Show - S01E01.sample.mkv",
        "Show/.@__thumb/Show - S01E02.mkv",
        "Show/@eaDir/Show - S01E03.mkv",
        "@Recycle/Old Show/Season 1/Old Show - S01E01.mkv",
    ] {
        touch(&tv_root, rel);
    }

    scan(&fx.pool, &film).await;
    scan(&fx.pool, &tv).await;

    assert_eq!(
        paths(&fx.pool, &film, "Movie").await,
        [
            "Amadeus (1984)/Amadeus (1984).mkv",
            "Amadeus (1984)/Featurettes/Making Of.mkv",
            "Argo (2012)/Argo (2012).mkv",
            "Sample Movie (2010)/Sample Movie (2010).mkv",
        ]
    );
    assert_eq!(
        paths(&fx.pool, &tv, "Episode").await,
        ["Show/Season 1/Show - S01E01.mkv"]
    );
    assert_eq!(paths(&fx.pool, &tv, "Season").await, ["Show/Season 1"]);
    assert_eq!(paths(&fx.pool, &tv, "Series").await, ["Show"]);
}

#[tokio::test]
async fn scan_root_may_start_with_dot() {
    let fx = Fixture::new().await;
    let lib = fx
        .library(".media/@films", LibraryContentType::Movies)
        .await;
    touch(&root_of(&lib), "Argo (2012)/Argo (2012).mkv");

    scan(&fx.pool, &lib).await;

    assert_eq!(
        paths(&fx.pool, &lib, "Movie").await,
        ["Argo (2012)/Argo (2012).mkv"]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn scan_skips_symlink_loop() {
    use std::os::unix::fs::symlink;
    let fx = Fixture::new().await;

    // A folder holding a link to itself. The old scanner stored nested copies through it.
    let own = fx.library("loop-self", LibraryContentType::Movies).await;
    let own_root = root_of(&own);
    touch(&own_root, "Loop Test (2020)/Loop Test (2020).mkv");
    symlink(".", own_root.join("Loop Test (2020)/loop")).unwrap();
    for rel in [
        "Loop Test (2020)/loop/Loop Test (2020).mkv",
        "Loop Test (2020)/loop/loop/Loop Test (2020).mkv",
    ] {
        insert_item(&fx.pool, &own, "Movie", &own_root.join(rel), None).await;
    }

    // A folder holding a link back to the library root.
    let up = fx.library("loop-root", LibraryContentType::Movies).await;
    let up_root = root_of(&up);
    touch(&up_root, "Other (2001)/Other (2001).mkv");
    symlink(&up_root, up_root.join("Other (2001)/root")).unwrap();

    scan(&fx.pool, &own).await;
    scan(&fx.pool, &up).await;

    assert_eq!(
        paths(&fx.pool, &own, "Movie").await,
        ["Loop Test (2020)/Loop Test (2020).mkv"]
    );
    assert_eq!(
        paths(&fx.pool, &up, "Movie").await,
        ["Other (2001)/Other (2001).mkv"]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn rescan_removes_items_deep_in_symlink_loop() {
    use std::os::unix::fs::symlink;
    let fx = Fixture::new().await;
    let lib = fx.library("film", LibraryContentType::Movies).await;
    let root = root_of(&lib);
    touch(&root, "Loop Test (2020)/Loop Test (2020).mkv");
    symlink(".", root.join("Loop Test (2020)/loop")).unwrap();
    // A copy stored through the loop so many times that the path no longer resolves.
    let deep = root
        .join("Loop Test (2020)")
        .join("loop/".repeat(45))
        .join("Loop Test (2020).mkv");
    assert!(
        std::fs::metadata(&deep).is_err(),
        "the test needs a path that does not resolve: {}",
        deep.display()
    );
    insert_item(&fx.pool, &lib, "Movie", &deep, None).await;

    scan(&fx.pool, &lib).await;

    assert_eq!(
        paths(&fx.pool, &lib, "Movie").await,
        ["Loop Test (2020)/Loop Test (2020).mkv"]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn scan_follows_two_links_to_one_dir() {
    use std::os::unix::fs::symlink;
    let fx = Fixture::new().await;
    let store = fx.path("store");
    touch(&store, "Shared (2005)/Shared (2005).mkv");
    touch(&store, "clip.mkv");
    let lib = fx.library("film", LibraryContentType::Movies).await;
    let root = root_of(&lib);
    for dir in ["A", "B"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
        symlink(
            store.join("Shared (2005)"),
            root.join(dir).join("Shared (2005)"),
        )
        .unwrap();
    }
    symlink(store.join("clip.mkv"), root.join("Linked (2006).mkv")).unwrap();

    scan(&fx.pool, &lib).await;

    assert_eq!(
        paths(&fx.pool, &lib, "Movie").await,
        [
            "A/Shared (2005)/Shared (2005).mkv",
            "B/Shared (2005)/Shared (2005).mkv",
            "Linked (2006).mkv",
        ]
    );
}

#[tokio::test]
async fn rescan_removes_items_now_ignored() {
    let fx = Fixture::new().await;
    let lib = fx.library("film", LibraryContentType::Movies).await;
    let root = root_of(&lib);
    touch(&root, "Argo (2012)/Argo (2012).mkv");
    let thumb = touch(&root, "Argo (2012)/.@__thumb/Argo (2012).mkv");
    let junk = touch(&root, "#recycle/Junk (2001).mkv");
    // Items the old scanner made for these files, and rows that point at them.
    let thumb_id = insert_item(&fx.pool, &lib, "Movie", &thumb, None)
        .await
        .to_string();
    let junk_id = insert_item(&fx.pool, &lib, "Movie", &junk, None)
        .await
        .to_string();
    let user = insert_user(&fx.pool).await;
    let pool = &fx.pool;
    exec(
        pool,
        "INSERT INTO user_item_data (user_id, item_id) VALUES (?1, ?2)",
        &[&user, &thumb_id],
    )
    .await;
    exec(
        pool,
        "INSERT INTO people (id, name) VALUES ('p1', 'Someone')",
        &[],
    )
    .await;
    exec(
        pool,
        "INSERT INTO item_people (item_id, person_id) VALUES (?1, 'p1')",
        &[&thumb_id],
    )
    .await;
    exec(
        pool,
        "INSERT INTO studios (id, name) VALUES ('s1', 'Studio')",
        &[],
    )
    .await;
    exec(
        pool,
        "INSERT INTO item_studios (item_id, studio_id) VALUES (?1, 's1')",
        &[&junk_id],
    )
    .await;

    for round in 1..=2 {
        scan(pool, &lib).await;
        assert_eq!(
            paths(pool, &lib, "Movie").await,
            ["Argo (2012)/Argo (2012).mkv"],
            "after scan {round}"
        );
    }
    assert_eq!(count(pool, "SELECT COUNT(*) FROM user_item_data").await, 0);
    assert_eq!(count(pool, "SELECT COUNT(*) FROM item_people").await, 0);
    assert_eq!(count(pool, "SELECT COUNT(*) FROM item_studios").await, 0);
    assert_eq!(count(pool, "SELECT COUNT(*) FROM people").await, 1);
}

#[tokio::test]
async fn rescan_keeps_items_with_missing_files() {
    let fx = Fixture::new().await;
    let lib = fx.library("film", LibraryContentType::Movies).await;
    let root = root_of(&lib);
    touch(&root, "Gone (1999)/Gone (1999).mkv");
    touch(&root, "Here (2000)/Here (2000).mkv");
    // An item outside the library's roots, whose file exists.
    let elsewhere = touch(&fx.path("elsewhere"), "Old (1990).mkv");
    insert_item(&fx.pool, &lib, "Movie", &elsewhere, None).await;
    scan(&fx.pool, &lib).await;

    std::fs::remove_dir_all(root.join("Gone (1999)")).unwrap();
    scan(&fx.pool, &lib).await;

    let mut expected = vec![
        "Gone (1999)/Gone (1999).mkv".to_string(),
        "Here (2000)/Here (2000).mkv".to_string(),
        elsewhere.to_str().unwrap().to_string(),
    ];
    expected.sort();
    assert_eq!(paths(&fx.pool, &lib, "Movie").await, expected);
}

#[cfg(unix)]
#[tokio::test]
async fn rescan_keeps_items_in_unreadable_dirs() {
    use std::os::unix::fs::PermissionsExt;

    /// Sets a folder's permission bits on drop, so the temp dir can be removed.
    struct Mode(PathBuf, u32);
    impl Drop for Mode {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(self.1));
        }
    }

    let fx = Fixture::new().await;
    let lib = fx.library("film", LibraryContentType::Movies).await;
    let root = root_of(&lib);
    touch(&root, "Argo (2012)/Argo (2012).mkv");
    touch(&root, "Locked (2001)/Locked (2001).mkv");
    scan(&fx.pool, &lib).await;

    // The folder cannot be listed, but its file can still be opened, as on a share that
    // fails part way through a scan.
    let locked = root.join("Locked (2001)");
    let _restore = Mode(locked.clone(), 0o755);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o100)).unwrap();
    if std::fs::read_dir(&locked).is_ok() {
        eprintln!("skip: the folder is still readable (running as root?)");
        return;
    }
    scan(&fx.pool, &lib).await;

    assert_eq!(
        paths(&fx.pool, &lib, "Movie").await,
        [
            "Argo (2012)/Argo (2012).mkv",
            "Locked (2001)/Locked (2001).mkv"
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_rescans_remove_items_in_both_libraries() {
    let fx = Fixture::new().await;
    let user = insert_user(&fx.pool).await;
    let mut libs = Vec::new();
    for name in ["film-a", "film-b"] {
        let lib = fx.library(name, LibraryContentType::Movies).await;
        let root = root_of(&lib);
        // The real file is already known, so the rescans only remove.
        let argo = touch(&root, "Argo (2012)/Argo (2012).mkv");
        insert_item(&fx.pool, &lib, "Movie", &argo, None).await;
        for i in 0..20 {
            let junk = touch(&root, &format!("Argo (2012)/.@__thumb/Argo {i}.mkv"));
            let id = insert_item(&fx.pool, &lib, "Movie", &junk, None)
                .await
                .to_string();
            exec(
                &fx.pool,
                "INSERT INTO user_item_data (user_id, item_id) VALUES (?1, ?2)",
                &[&user, &id],
            )
            .await;
        }
        libs.push(lib);
    }

    tokio::join!(scan(&fx.pool, &libs[0]), scan(&fx.pool, &libs[1]));

    for lib in &libs {
        assert_eq!(
            paths(&fx.pool, lib, "Movie").await,
            ["Argo (2012)/Argo (2012).mkv"],
            "library {}",
            lib.name
        );
    }
    assert_eq!(
        count(&fx.pool, "SELECT COUNT(*) FROM user_item_data").await,
        0
    );
}

#[tokio::test]
async fn rescan_removes_empty_tv_seasons_and_series() {
    let fx = Fixture::new().await;
    let lib = fx.library("tv", LibraryContentType::TvShows).await;
    let root = root_of(&lib);
    touch(&root, "Show/Season 1/Show - S01E01.mkv");
    let thumb = touch(&root, "Show/.@__thumb/Show - S01E01.mkv");
    let junk = touch(&root, "#recycle/Junk.mkv");
    // What the old scanner made of the two junk files.
    let pool = &fx.pool;
    let show = insert_item(pool, &lib, "Series", &root.join("Show"), None).await;
    let thumb_season = insert_item(
        pool,
        &lib,
        "Season",
        &root.join("Show/.@__thumb"),
        Some(show),
    )
    .await;
    insert_item(pool, &lib, "Episode", &thumb, Some(thumb_season)).await;
    let recycle = insert_item(pool, &lib, "Series", &root.join("#recycle"), None).await;
    let recycle_season = insert_item(
        pool,
        &lib,
        "Season",
        &root.join("#recycle/Season 1"),
        Some(recycle),
    )
    .await;
    let junk_ep = insert_item(pool, &lib, "Episode", &junk, Some(recycle_season)).await;
    let user = insert_user(pool).await;
    exec(
        pool,
        "INSERT INTO pinned_episodes (user_id, series_id, episode_id, season_id, created_date)
         VALUES (?1, ?2, ?3, ?4, datetime('now'))",
        &[
            &user,
            &recycle.to_string(),
            &junk_ep.to_string(),
            &recycle_season.to_string(),
        ],
    )
    .await;
    exec(
        pool,
        "INSERT INTO user_item_data (user_id, item_id) VALUES (?1, ?2)",
        &[&user, &junk_ep.to_string()],
    )
    .await;

    scan(pool, &lib).await;

    assert_eq!(
        paths(pool, &lib, "Episode").await,
        ["Show/Season 1/Show - S01E01.mkv"]
    );
    assert_eq!(paths(pool, &lib, "Season").await, ["Show/Season 1"]);
    assert_eq!(paths(pool, &lib, "Series").await, ["Show"]);
    assert_eq!(count(pool, "SELECT COUNT(*) FROM pinned_episodes").await, 0);
    assert_eq!(count(pool, "SELECT COUNT(*) FROM user_item_data").await, 0);
}

#[tokio::test]
async fn tv_scan_sets_episode_and_season_numbers() {
    let fx = Fixture::new().await;
    let lib = fx.library("tv", LibraryContentType::TvShows).await;
    let root = root_of(&lib);
    for rel in [
        "Severance/Season 2/Severance - S02E01 - Hello, Ms. Cobel.mkv",
        "Severance/Season 2/Severance - S02E02E03 - Goodbye.mkv",
        // The season folder's number wins over the file's.
        "Show/Season 1/Show S03E04.mkv",
        // A season folder without a number: the file's season.
        "Show/Specials/Show S00E01.mkv",
        // Directly in the series folder: the season is the file's, else 1.
        "Flat Show/Flat Show 3x07.mkv",
        "Flat Show/Flat Show - Episode 9.mkv",
        "Flat Show/Pilot.mkv",
    ] {
        touch(&root, rel);
    }

    scan(&fx.pool, &lib).await;

    let row = |path: &str, index: Option<i64>, season: Option<i64>, season_name: &str| {
        (path.to_string(), index, season, season_name.to_string())
    };
    assert_eq!(
        episodes(&fx.pool, &lib).await,
        [
            row(
                "Flat Show/Flat Show - Episode 9.mkv",
                Some(9),
                Some(1),
                "Season 1"
            ),
            row("Flat Show/Flat Show 3x07.mkv", Some(7), Some(3), "Season 3"),
            row("Flat Show/Pilot.mkv", None, Some(1), "Season 1"),
            row(
                "Severance/Season 2/Severance - S02E01 - Hello, Ms. Cobel.mkv",
                Some(1),
                Some(2),
                "Season 2"
            ),
            row(
                "Severance/Season 2/Severance - S02E02E03 - Goodbye.mkv",
                Some(2),
                Some(2),
                "Season 2"
            ),
            row(
                "Show/Season 1/Show S03E04.mkv",
                Some(4),
                Some(1),
                "Season 1"
            ),
            row(
                "Show/Specials/Show S00E01.mkv",
                Some(1),
                Some(0),
                "Specials"
            ),
        ]
    );
}
