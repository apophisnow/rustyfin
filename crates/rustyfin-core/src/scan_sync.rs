//! Removal of items that a library scan no longer collects, and the helper that deletes an item.
//! All SQL that a scan uses to delete items lives here.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rustyfin_model::library::{Library, LibraryContentType};
use sqlx::SqlitePool;

/// Deletes the item `id` and the rows that refer to it, in one transaction.
///
/// An item that still has children is not deleted: this logs a warning and returns `Ok(false)`.
/// Returns `Ok(true)` when the item was deleted.
pub async fn delete_item(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let children: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_items WHERE parent_id = ?1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    if children > 0 {
        tracing::warn!("not removing item {id}: it has {children} child items");
        return Ok(false);
    }
    // These references to media_items have no ON DELETE CASCADE, so they go first.
    sqlx::query("DELETE FROM pinned_episodes WHERE series_id=?1 OR episode_id=?1 OR season_id=?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM user_item_data WHERE item_id=?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM item_people WHERE item_id=?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM item_studios WHERE item_id=?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM media_items WHERE id=?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// Deletes the items of `library` that the scan did not collect, and returns how many were deleted.
///
/// Step 1 covers the media types that the library's content type scans. An item is deleted only
/// when all of these hold: its path is under a library root; `fs::metadata` says it is a regular
/// file, so a missing or unreadable file keeps it; its path is not in `collected`; and no folder
/// in `unreadable` contains it. Step 2, for TV libraries only, then deletes the Seasons and then
/// the Series that have no children.
pub async fn remove_not_collected(
    pool: &SqlitePool,
    library: &Library,
    collected: &HashSet<String>,
    unreadable: &[PathBuf],
) -> Result<u32, sqlx::Error> {
    let library_id = library.id.to_string();
    let roots: Vec<PathBuf> = library.paths.iter().map(PathBuf::from).collect();
    let mut removed = 0;

    for &media_type in scanned_types(&library.content_type) {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, path FROM media_items WHERE library_id = ?1 AND media_type = ?2",
        )
        .bind(library_id.as_str())
        .bind(media_type)
        .fetch_all(pool)
        .await?;
        for (id, path) in rows {
            let item = Path::new(&path);
            let under_root = roots.iter().any(|root| item.starts_with(root));
            let unlisted = unreadable.iter().any(|dir| item.starts_with(dir));
            if !under_root || unlisted || collected.contains(&path) {
                continue;
            }
            if !is_regular_file(item).await {
                continue;
            }
            if delete_item(pool, &id).await? {
                removed += 1;
            }
        }
    }

    if matches!(library.content_type, LibraryContentType::TvShows) {
        // Seasons first: deleting one can leave its series with no children.
        for media_type in ["Season", "Series"] {
            let ids: Vec<String> = sqlx::query_scalar(
                "SELECT id FROM media_items AS m
                 WHERE m.library_id = ?1 AND m.media_type = ?2
                   AND NOT EXISTS (SELECT 1 FROM media_items AS c WHERE c.parent_id = m.id)",
            )
            .bind(library_id.as_str())
            .bind(media_type)
            .fetch_all(pool)
            .await?;
            for id in ids {
                if delete_item(pool, &id).await? {
                    removed += 1;
                }
            }
        }
    }

    Ok(removed)
}

/// The media types that a scan of a library with this content type collects.
/// Music is not listed, because its scan removes nothing.
fn scanned_types(content_type: &LibraryContentType) -> &'static [&'static str] {
    match content_type {
        LibraryContentType::Movies => &["Movie"],
        LibraryContentType::Photos => &["Photo"],
        LibraryContentType::Mixed => &["Movie", "Audio", "Photo"],
        LibraryContentType::TvShows => &["Episode"],
        LibraryContentType::Music => &[],
    }
}

/// Whether `fs::metadata` says that `path` is a regular file. A path that cannot be read is not.
async fn is_regular_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|meta| meta.is_file())
}
