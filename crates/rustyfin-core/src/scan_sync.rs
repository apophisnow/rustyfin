//! Removal of items that a library scan no longer collects, the helper that deletes an item, and the
//! reconciliation of movie and episode version groups with the database.
//! All SQL that a scan uses to write items lives here.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use chrono::{DateTime, FixedOffset, Utc};
use rustyfin_db::repo_media::PathScope;
use rustyfin_model::library::{Library, LibraryContentType};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::naming::{VersionGroup, is_ignored_dir, is_ignored_file};
use crate::probe::ProbeStep;
use crate::scanner::parse_year;

/// Deletes the item `id` and the rows that refer to it, in one transaction.
///
/// An item that still has children is not deleted: this logs a warning and returns `Ok(false)`.
/// Returns `Ok(true)` when the item was deleted.
pub async fn delete_item(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    // The write lock is taken first: under the shared cache, a read-then-write transaction can
    // deadlock with another one, and the busy timeout does not help.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let deleted = delete_item_in(&mut tx, id).await?;
    tx.commit().await?;
    Ok(deleted)
}

/// Deletes the item `id` on `conn`, which the caller holds in a write transaction, so that the
/// caller can merge other rows into the item first. Otherwise as [`delete_item`].
pub async fn delete_item_in(conn: &mut SqliteConnection, id: &str) -> Result<bool, sqlx::Error> {
    let children: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_items WHERE parent_id = ?1")
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
    if children > 0 {
        tracing::warn!("not removing item {id}: it has {children} child items");
        return Ok(false);
    }
    // These references to media_items have no ON DELETE CASCADE, so they go first.
    sqlx::query("DELETE FROM pinned_episodes WHERE series_id=?1 OR episode_id=?1 OR season_id=?1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM user_item_data WHERE item_id=?1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM item_people WHERE item_id=?1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM item_studios WHERE item_id=?1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM media_items WHERE id=?1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(true)
}

/// Deletes the items of `library` whose path is in `scope` that the scan did not collect, and returns how
/// many were deleted. `PathScope::All` covers the whole library.
///
/// Step 1 covers the media types that the library's content type scans. An item is deleted only
/// when all of these hold: its path is under a library root; its path is not in `collected`; no
/// path in `unreadable` contains it; and either its path is under a folder the scan skipped as a
/// symlink loop (such a path is never collected again and may not resolve), or its file is a
/// regular file, or its file is missing. A missing file is deleted only while its root lists
/// something that naming does not ignore: a root that lists nothing may be unmounted, so nothing
/// missing under it is deleted, and a warning is logged once for each such root.
/// Step 2 then deletes the containers with no children: the Seasons and Series of a TV library, and
/// the MusicAlbums of a music library, which hold both albums and artists. It repeats until a pass
/// deletes nothing, because a deleted container can leave its parent without children. A scope other
/// than `PathScope::All` runs step 2 over the parents of the items it deleted and the containers in
/// scope instead (see `remove_empty_containers_in_scope`).
pub async fn remove_not_collected(
    pool: &SqlitePool,
    library: &Library,
    scope: &PathScope,
    collected: &HashSet<String>,
    unreadable: &[PathBuf],
    loops: &[PathBuf],
) -> Result<u32, sqlx::Error> {
    let library_id = library.id.to_string();
    let roots: Vec<PathBuf> = library.paths.iter().map(PathBuf::from).collect();
    // Looked up by each item's ancestors: with a dangling link per item, as when a mount of links
    // drops, the list is as long as the library.
    let unreadable: HashSet<&Path> = unreadable.iter().map(PathBuf::as_path).collect();
    // Whether each root lists something, looked up once per root, and the roots already warned about.
    let mut listed: HashMap<usize, bool> = HashMap::new();
    let mut warned: HashSet<usize> = HashSet::new();
    // The parents of the items that step 1 deletes.
    let mut parents: HashSet<String> = HashSet::new();
    let mut removed = 0;

    for &media_type in scanned_types(&library.content_type) {
        let rows: Vec<(String, String, Option<String>)> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT id, path, parent_id FROM media_items
                 WHERE library_id = ?1 AND media_type = ?2 AND {}",
                scope.sql("path", 3)
            )))
            .bind(library_id.as_str())
            .bind(media_type)
            .bind(scope.param())
            .fetch_all(pool)
            .await?;
        for (id, path, parent_id) in rows {
            let item = Path::new(&path);
            let Some(root) = roots
                .iter()
                .position(|candidate| item.starts_with(candidate))
            else {
                continue;
            };
            let unlisted = item.ancestors().any(|dir| unreadable.contains(dir));
            if unlisted || collected.contains(&path) {
                continue;
            }
            let in_loop = loops.iter().any(|dir| item.starts_with(dir));
            // A path that is neither a regular file nor under a loop stays, unless its file is
            // missing and its root lists something.
            if !in_loop && !is_regular_file(item).await {
                if !is_gone(item).await {
                    continue;
                }
                let root_listed = match listed.get(&root) {
                    Some(&known) => known,
                    None => {
                        let known = root_is_listed(&roots[root]).await;
                        listed.insert(root, known);
                        known
                    }
                };
                if !root_listed {
                    if warned.insert(root) {
                        tracing::warn!(
                            "not removing the missing items under {}: the folder lists nothing, so it may not be mounted",
                            roots[root].display()
                        );
                    }
                    continue;
                }
            }
            if delete_item(pool, &id).await? {
                removed += 1;
                parents.extend(parent_id);
            }
        }
    }

    // Seasons come before their series, and albums before their artists, which are MusicAlbums too.
    let containers: &[&str] = match library.content_type {
        LibraryContentType::TvShows => &["Season", "Series"],
        LibraryContentType::Music => &["MusicAlbum"],
        _ => &[],
    };
    if !matches!(scope, PathScope::All) {
        removed +=
            remove_empty_containers_in_scope(pool, &library_id, scope, containers, parents).await?;
        return Ok(removed);
    }
    loop {
        let mut deleted_in_pass: u32 = 0;
        for &media_type in containers {
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
                    deleted_in_pass += 1;
                }
            }
        }
        if deleted_in_pass == 0 {
            break;
        }
        removed += deleted_in_pass;
    }

    Ok(removed)
}

/// Deletes the childless containers that a scoped removal reaches: the childless ones with a path in
/// `scope`, and the `candidates`, which are the parents of the items the removal deleted. A deleted container
/// makes its parent a candidate, so the removal goes up the tree; a container that loses its last child is
/// reached that way too. It stops after a pass that deletes nothing. Returns how many items were deleted.
async fn remove_empty_containers_in_scope(
    pool: &SqlitePool,
    library_id: &str,
    scope: &PathScope,
    containers: &[&str],
    mut candidates: HashSet<String>,
) -> Result<u32, sqlx::Error> {
    for &media_type in containers {
        let ids: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT m.id FROM media_items AS m
             WHERE m.library_id = ?1 AND m.media_type = ?2 AND {}
               AND NOT EXISTS (SELECT 1 FROM media_items AS c WHERE c.parent_id = m.id)",
            scope.sql("m.path", 3)
        )))
        .bind(library_id)
        .bind(media_type)
        .bind(scope.param())
        .fetch_all(pool)
        .await?;
        candidates.extend(ids);
    }
    let mut removed = 0;
    while !candidates.is_empty() {
        let mut next: HashSet<String> = HashSet::new();
        let mut deleted_in_pass: u32 = 0;
        let batch: Vec<String> = candidates.drain().collect();
        for id in batch {
            // The row is read again: an earlier deletion in this pass may have removed it already.
            let row: Option<(String, Option<String>)> = sqlx::query_as(
                "SELECT m.media_type, m.parent_id FROM media_items AS m
                 WHERE m.id = ?1
                   AND NOT EXISTS (SELECT 1 FROM media_items AS c WHERE c.parent_id = m.id)",
            )
            .bind(id.as_str())
            .fetch_optional(pool)
            .await?;
            let Some((media_type, parent_id)) = row else {
                continue;
            };
            if containers.contains(&media_type.as_str()) && delete_item(pool, &id).await? {
                deleted_in_pass += 1;
                next.extend(parent_id);
            }
        }
        if deleted_in_pass == 0 {
            break;
        }
        removed += deleted_in_pass;
        candidates = next;
    }
    Ok(removed)
}

/// The media types that a scan of a library with this content type collects.
fn scanned_types(content_type: &LibraryContentType) -> &'static [&'static str] {
    match content_type {
        LibraryContentType::Movies => &["Movie"],
        LibraryContentType::Photos => &["Photo"],
        LibraryContentType::Mixed => &["Movie", "Audio", "Photo"],
        LibraryContentType::TvShows => &["Episode"],
        LibraryContentType::Music => &["Audio"],
    }
}

/// Whether `fs::metadata` says that `path` is a regular file. A path that cannot be read is not.
async fn is_regular_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|meta| meta.is_file())
}

/// Whether the file at `path` is gone: `fs::symlink_metadata` says that the entry does not exist. A
/// dangling link is not gone, since the link itself is there. Any other error leaves the file in place.
async fn is_gone(path: &Path) -> bool {
    tokio::fs::symlink_metadata(path)
        .await
        .is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound)
}

/// Whether the library folder `root` looks mounted: it lists at least one entry that naming does not
/// ignore and that resolves, so a dangling link does not count. A missing or unreadable folder does
/// not, and nor does a folder with only ignored entries or only dangling links.
async fn root_is_listed(root: &Path) -> bool {
    let Ok(mut entries) = tokio::fs::read_dir(root).await else {
        return false;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !is_ignored_dir(&name)
            && !is_ignored_file(&name)
            && tokio::fs::metadata(entry.path()).await.is_ok()
        {
            return true;
        }
    }
    false
}

/// The size and modification time of the file at `path`, the modification time in whole seconds
/// since the Unix epoch. `std::fs::metadata` follows symlinks. `None` when either cannot be read.
pub(crate) fn file_stamp(path: &str) -> Option<(i64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((
        i64::try_from(meta.len()).ok()?,
        i64::try_from(modified.as_secs()).ok()?,
    ))
}

/// Whether the file at `path` no longer has the size and modification time stored with its source.
/// A source with no stored modification time is not changed: it is stamped without a probe. A file
/// that cannot be read now is not changed either, so its sources are kept.
pub(crate) fn source_changed(path: &str, size: Option<i64>, modified: Option<i64>) -> bool {
    let Some(modified) = modified else {
        return false;
    };
    let Some(stamp) = file_stamp(path) else {
        return false;
    };
    size.map(|size| (size, modified)) != Some(stamp)
}

/// Whether `file` is a known failure: it has a probe failure on record, and it is still the file
/// that failed, with the same size and modification time. Only a file with a record is stat'ed.
fn is_known_failure(file: &str, failures: &HashMap<String, (i64, i64)>) -> bool {
    failures
        .get(file)
        .is_some_and(|&stamp| file_stamp(file) == Some(stamp))
}

/// Records that the probe of `path` failed, with `stamp`: the file's size and modification time from
/// before the probe, so that a file that changed during the probe is probed again. Does nothing when
/// `stamp` is `None`, that is when the file's metadata could not be read.
pub(crate) async fn record_probe_failure(
    pool: &SqlitePool,
    path: &str,
    stamp: Option<(i64, i64)>,
) -> Result<(), sqlx::Error> {
    let Some((size, modified)) = stamp else {
        return Ok(());
    };
    sqlx::query(
        "INSERT INTO probe_failures (path, size, modified, failed_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(path) DO UPDATE SET
           size = excluded.size,
           modified = excluded.modified,
           failed_at = excluded.failed_at",
    )
    .bind(path)
    .bind(size)
    .bind(modified)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

/// Forgets the probe failure of `path`, if it has one.
pub(crate) async fn clear_probe_failure(pool: &SqlitePool, path: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM probe_failures WHERE path = ?1")
        .bind(path)
        .execute(pool)
        .await?;
    Ok(())
}

/// Stores `stamp`, the size and modification time of the file at `path`, on the source of `item_id`
/// for that path. Does nothing when `stamp` is `None`, that is when the file could not be read.
pub(crate) async fn store_stamp(
    pool: &SqlitePool,
    item_id: &str,
    path: &str,
    stamp: Option<(i64, i64)>,
) -> Result<(), sqlx::Error> {
    let Some((size, modified)) = stamp else {
        return Ok(());
    };
    sqlx::query(
        "UPDATE media_sources SET size = ?1, modified = ?2 WHERE item_id = ?3 AND path = ?4",
    )
    .bind(size)
    .bind(modified)
    .bind(item_id)
    .bind(path)
    .execute(pool)
    .await?;
    Ok(())
}

/// An item of the library, as [`reconcile_groups`] reads it.
#[derive(sqlx::FromRow)]
struct ItemRow {
    id: String,
    path: String,
    name: String,
    sort_name: Option<String>,
    production_year: Option<i32>,
    runtime_ticks: Option<i64>,
    date_created: String,
    index_number: Option<i32>,
    parent_index_number: Option<i32>,
    parent_id: Option<String>,
}

/// A source of an item, as [`reconcile_groups`] reads it.
#[derive(sqlx::FromRow)]
struct SourceRow {
    item_id: String,
    path: String,
    run_time_ticks: Option<i64>,
    /// The size and modification time of the file when the source was stamped.
    size: Option<i64>,
    modified: Option<i64>,
}

/// The fields that a group gives its item, by media type.
enum Wanted {
    /// A movie: its title and year.
    Movie { title: String, year: Option<i32> },
    /// An episode: the stem of its first file, its numbers, and its season.
    Episode {
        name: String,
        index_number: Option<i32>,
        parent_index_number: Option<i32>,
        parent_id: Uuid,
    },
}

/// A group that [`reconcile_groups`] settles: its files in version order, and the fields it gives.
struct WantedGroup {
    files: Vec<String>,
    wanted: Wanted,
}

/// What a scan does with one version group, as decided by [`reconcile_movie_groups`] and
/// [`reconcile_episode_groups`].
#[derive(Debug)]
pub enum GroupOutcome {
    /// The group's item exists. `steps` is empty when the item needs no probing.
    Existing {
        item_id: Uuid,
        steps: Vec<ProbeStep>,
    },
    /// The group has no item yet. The scan creates one at `path`, the group's primary, then runs
    /// `steps` on it.
    New { path: String, steps: Vec<ProbeStep> },
}

/// Matches each movie version group with its item, merges the other owners of a group into the
/// survivor, updates the survivor's path and names, and says what to probe. The outcomes are in the
/// order of `groups`. `scope` is the part of the library that the scan collected: only the items in it
/// are read. Call this before the removal of items that are no longer collected.
pub async fn reconcile_movie_groups(
    pool: &SqlitePool,
    library_id: Uuid,
    scope: &PathScope,
    groups: &[VersionGroup],
) -> Result<Vec<GroupOutcome>, sqlx::Error> {
    let wanted: Vec<WantedGroup> = groups
        .iter()
        .map(|group| WantedGroup {
            files: strings_of(&group.files),
            wanted: Wanted::Movie {
                title: group.title.clone(),
                year: group.year,
            },
        })
        .collect();
    reconcile_groups(pool, library_id, "Movie", scope, &wanted).await
}

/// One episode's files in version order, and the fields that the episode gives its item.
pub struct EpisodeGroup {
    pub files: Vec<PathBuf>,
    pub season_id: Uuid,
    pub index_number: Option<i32>,
    pub parent_index_number: Option<i32>,
}

/// Matches each episode group with its item, as [`reconcile_movie_groups`] does for movies. The
/// outcomes are in the order of `groups`. The name and sort name of an episode come from the stem
/// of its first file. `scope` is the part of the library that the scan collected: only the items in it
/// are read. Call this before the removal of items that are no longer collected.
pub async fn reconcile_episode_groups(
    pool: &SqlitePool,
    library_id: Uuid,
    scope: &PathScope,
    groups: &[EpisodeGroup],
) -> Result<Vec<GroupOutcome>, sqlx::Error> {
    let wanted: Vec<WantedGroup> = groups
        .iter()
        .map(|group| WantedGroup {
            files: strings_of(&group.files),
            wanted: Wanted::Episode {
                name: group
                    .files
                    .first()
                    .and_then(|file| file.file_stem())
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                index_number: group.index_number,
                parent_index_number: group.parent_index_number,
                parent_id: group.season_id,
            },
        })
        .collect();
    reconcile_groups(pool, library_id, "Episode", scope, &wanted).await
}

/// Settles the groups of one media type in the library. The outcomes are in the order of `groups`.
///
/// Groups are handled in order of their first file, and the writes for one group run in one
/// transaction. Only the items with a path or a source in `scope` are read, with all the sources of
/// those items. Only the probe failures in `scope` are read, since every file of `groups` is in `scope`.
async fn reconcile_groups(
    pool: &SqlitePool,
    library_id: Uuid,
    media_type: &str,
    scope: &PathScope,
    groups: &[WantedGroup],
) -> Result<Vec<GroupOutcome>, sqlx::Error> {
    let library = library_id.to_string();
    // An item is loaded when its path is in `scope`, or when one of its sources is.
    let loaded = format!(
        "({} OR EXISTS (SELECT 1 FROM media_sources AS q WHERE q.item_id = m.id AND {}))",
        scope.sql("m.path", 3),
        scope.sql("q.path", 3)
    );
    let items: Vec<ItemRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, path, name, sort_name, production_year, runtime_ticks, date_created,
                index_number, parent_index_number, parent_id
         FROM media_items AS m
         WHERE m.library_id = ?1 AND m.media_type = ?2 AND {loaded}"
    )))
    .bind(library.as_str())
    .bind(media_type)
    .bind(scope.param())
    .fetch_all(pool)
    .await?;
    let sources: Vec<SourceRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT s.item_id, s.path, s.run_time_ticks, s.size, s.modified
         FROM media_sources AS s
         JOIN media_items AS m ON m.id = s.item_id
         WHERE m.library_id = ?1 AND m.media_type = ?2 AND {loaded}
         ORDER BY s.rowid"
    )))
    .bind(library.as_str())
    .bind(media_type)
    .bind(scope.param())
    .fetch_all(pool)
    .await?;

    // The files whose probe failed, with the size and modification time that they had then.
    let failures: HashMap<String, (i64, i64)> =
        sqlx::query_as::<_, (String, i64, i64)>(sqlx::AssertSqlSafe(format!(
            "SELECT path, size, modified FROM probe_failures WHERE {}",
            scope.sql("path", 1)
        )))
        .bind(scope.param())
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|(path, size, modified)| (path, (size, modified)))
        .collect();

    let item_by_id: HashMap<&str, &ItemRow> = items.iter().map(|i| (i.id.as_str(), i)).collect();
    let mut by_path: HashMap<&str, Vec<&ItemRow>> = HashMap::new();
    for item in &items {
        by_path.entry(item.path.as_str()).or_default().push(item);
    }
    let mut by_source: HashMap<&str, Vec<&ItemRow>> = HashMap::new();
    let mut sources_of: HashMap<&str, Vec<&SourceRow>> = HashMap::new();
    for row in &sources {
        sources_of
            .entry(row.item_id.as_str())
            .or_default()
            .push(row);
        if let Some(&item) = item_by_id.get(row.item_id.as_str()) {
            by_source.entry(row.path.as_str()).or_default().push(item);
        }
    }
    let mut group_paths: HashSet<&str> = groups
        .iter()
        .flat_map(|group| group.files.iter())
        .map(String::as_str)
        .collect();
    // An item outside the scope is loaded for its sources. A full scan holds its path in `group_paths`
    // while its file exists, so it is not an owner of a group by source here either.
    for item in &items {
        if !scope.contains(&item.path) && is_regular_file(Path::new(&item.path)).await {
            group_paths.insert(item.path.as_str());
        }
    }

    let mut order: Vec<usize> = (0..groups.len()).collect();
    order.sort_by_key(|&idx| groups[idx].files.first());
    let mut claimed: HashSet<&str> = HashSet::new();
    let mut outcomes: Vec<(usize, GroupOutcome)> = Vec::with_capacity(groups.len());
    for idx in order {
        let group = &groups[idx];
        // The files that are not known failures, in order. The primary is the first of them, or the
        // group's first file when none is left.
        let probeable: Vec<String> = group
            .files
            .iter()
            .filter(|file| !is_known_failure(file, &failures))
            .cloned()
            .collect();
        let primary = probeable.first().unwrap_or(&group.files[0]);
        let owners = owners_of(
            &group.files,
            primary,
            &by_path,
            &by_source,
            &group_paths,
            &claimed,
        );
        let outcome = match owners.split_first() {
            None => GroupOutcome::New {
                path: primary.clone(),
                steps: group_steps(&probeable),
            },
            Some((&survivor, others)) => {
                claimed.extend(owners.iter().map(|owner| owner.id.as_str()));
                let survivor_sources = sources_of
                    .get(survivor.id.as_str())
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                settle_group(
                    pool,
                    group,
                    &probeable,
                    primary,
                    survivor,
                    others,
                    survivor_sources,
                )
                .await?
            }
        };
        outcomes.push((idx, outcome));
    }
    outcomes.sort_by_key(|(idx, _)| *idx);
    Ok(outcomes.into_iter().map(|(_, outcome)| outcome).collect())
}

/// The items that own a group, sorted with the item at `primary` first, then by creation date, and
/// without repeats. An owner is an item whose path is one of `files`, or an unclaimed item whose
/// path is in no group of this scan and that has a source among `files`.
fn owners_of<'a>(
    files: &[String],
    primary: &str,
    by_path: &HashMap<&str, Vec<&'a ItemRow>>,
    by_source: &HashMap<&str, Vec<&'a ItemRow>>,
    group_paths: &HashSet<&str>,
    claimed: &HashSet<&str>,
) -> Vec<&'a ItemRow> {
    let mut owners: Vec<&'a ItemRow> = Vec::new();
    for file in files {
        owners.extend(by_path.get(file.as_str()).into_iter().flatten().copied());
        for &item in by_source.get(file.as_str()).into_iter().flatten() {
            if !claimed.contains(item.id.as_str()) && !group_paths.contains(item.path.as_str()) {
                owners.push(item);
            }
        }
    }
    owners.sort_by(|a, b| {
        (a.path != primary, created_at(&a.date_created), &a.id).cmp(&(
            b.path != primary,
            created_at(&b.date_created),
            &b.id,
        ))
    });
    owners.dedup_by(|a, b| a.id == b.id);
    owners
}

/// Settles one group whose owners are known. Merges the other owners into the survivor, writes the
/// survivor's fields that changed (its path becomes `primary`), stamps the sources that have no
/// stamp yet, and works out what to probe: the `probeable` files, when the group is not steady.
async fn settle_group(
    pool: &SqlitePool,
    group: &WantedGroup,
    probeable: &[String],
    primary: &str,
    survivor: &ItemRow,
    others: &[&ItemRow],
    sources: &[&SourceRow],
) -> Result<GroupOutcome, sqlx::Error> {
    // The survivor's path and names before this scan. A name that the old code made from the file
    // stem is rewritten; a name that metadata set is kept.
    let old_stem = Path::new(&survivor.path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned());
    let old_stem_lower = old_stem.as_ref().map(|stem| stem.to_lowercase());
    let old_year = old_stem.as_deref().and_then(parse_year);

    let given_name = match &group.wanted {
        Wanted::Movie { title, .. } => title.as_str(),
        Wanted::Episode { name, .. } => name.as_str(),
    };
    let name = if old_stem.as_deref() == Some(survivor.name.as_str()) {
        given_name.to_string()
    } else {
        survivor.name.clone()
    };
    let sort_name = if survivor.sort_name.is_none() || survivor.sort_name == old_stem_lower {
        Some(given_name.to_lowercase())
    } else {
        survivor.sort_name.clone()
    };
    // A movie takes its year and keeps its episode fields. An episode keeps its year and takes the
    // group's numbers and season.
    let (production_year, index_number, parent_index_number, parent_id) = match &group.wanted {
        Wanted::Movie { year, .. } => (
            if survivor.production_year.is_none() || survivor.production_year == old_year {
                *year
            } else {
                survivor.production_year
            },
            survivor.index_number,
            survivor.parent_index_number,
            survivor.parent_id.clone(),
        ),
        Wanted::Episode {
            index_number,
            parent_index_number,
            parent_id: season,
            ..
        } => (
            survivor.production_year,
            *index_number,
            *parent_index_number,
            Some(season.to_string()),
        ),
    };
    let fields_changed = survivor.path != primary
        || name != survivor.name
        || sort_name != survivor.sort_name
        || production_year != survivor.production_year
        || index_number != survivor.index_number
        || parent_index_number != survivor.parent_index_number
        || parent_id != survivor.parent_id;

    if !others.is_empty() || fields_changed {
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        for other in others {
            merge_references(&mut tx, &survivor.id, &other.id).await?;
            delete_item_in(&mut tx, &other.id).await?;
        }
        if fields_changed {
            sqlx::query(
                "UPDATE media_items SET path = ?1, name = ?2, sort_name = ?3, production_year = ?4,
                 index_number = ?5, parent_index_number = ?6, parent_id = ?7
                 WHERE id = ?8",
            )
            .bind(primary)
            .bind(name.as_str())
            .bind(sort_name.as_deref())
            .bind(production_year)
            .bind(index_number)
            .bind(parent_index_number)
            .bind(parent_id.as_deref())
            .bind(survivor.id.as_str())
            .execute(&mut *tx)
            .await?;
        }
        // The pins of an episode follow its season. This runs after the merges, which have moved the
        // pins of the other owners onto the survivor.
        if let Wanted::Episode {
            parent_id: season, ..
        } = &group.wanted
        {
            sqlx::query("UPDATE pinned_episodes SET season_id = ?1 WHERE episode_id = ?2")
                .bind(season.to_string())
                .bind(survivor.id.as_str())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
    }

    // A source with no stored stamp is stamped now, without a probe: its probed data is kept.
    for source in sources.iter().filter(|source| source.modified.is_none()) {
        store_stamp(
            pool,
            &source.item_id,
            &source.path,
            file_stamp(&source.path),
        )
        .await?;
    }
    // Steady state: nothing else owns the group, the survivor is at the primary, its sources are the
    // probeable files, in order, with the right duration, and none of their files changed. Otherwise
    // they are probed again. A changed file makes the whole group probe again, so that its versions
    // stay in step. A primary with no duration cannot be fixed by probing, so it counts as right.
    let source_paths_match = sources
        .iter()
        .map(|source| source.path.as_str())
        .eq(probeable.iter().map(String::as_str));
    let runtime_match = sources.first().is_some_and(|first| {
        first.run_time_ticks.is_none() || first.run_time_ticks == survivor.runtime_ticks
    });
    let changed = sources
        .iter()
        .any(|source| source_changed(&source.path, source.size, source.modified));
    let steady = others.is_empty()
        && survivor.path == primary
        && source_paths_match
        && !changed
        && (probeable.is_empty() || runtime_match);
    let steps = if steady {
        Vec::new()
    } else {
        group_steps(probeable)
    };
    Ok(GroupOutcome::Existing {
        item_id: parse_id(&survivor.id)?,
        steps,
    })
}

/// One user's marks on one item, as `user_item_data` stores them.
struct Marks {
    position: i64,
    plays: i64,
    favorite: bool,
    played: bool,
    last_played: Option<String>,
    rating: Option<i64>,
    likes: Option<i64>,
}

/// The columns of a `Marks`, in the order `marks_on` reads them.
type MarksRow = (i64, i64, i64, i64, Option<String>, Option<i64>, Option<i64>);

/// Moves the references to item `other` over to item `survivor`. A user with a row on both copies
/// keeps one row on the survivor, which combines the two (see `combine`), and the other copy's row
/// is deleted. Other user data moves as it is, and so do pins. A collection or playlist that holds
/// both copies keeps the survivor's entry. The activity log, sessions and recordings that name
/// `other` name the survivor.
async fn merge_references(
    conn: &mut SqliteConnection,
    survivor: &str,
    other: &str,
) -> Result<(), sqlx::Error> {
    // The users with a row on both copies. Their rows combine before the move below, so the move
    // cannot hit a key that the survivor already has.
    let shared: Vec<String> = sqlx::query_scalar(
        "SELECT user_id FROM user_item_data WHERE item_id = ?1
         INTERSECT
         SELECT user_id FROM user_item_data WHERE item_id = ?2",
    )
    .bind(other)
    .bind(survivor)
    .fetch_all(&mut *conn)
    .await?;
    for user in shared {
        let ours = marks_on(conn, &user, survivor).await?;
        let theirs = marks_on(conn, &user, other).await?;
        let combined = combine(&ours, &theirs);
        sqlx::query(
            "UPDATE user_item_data SET playback_position_ticks = ?1, play_count = ?2,
             is_favorite = ?3, played = ?4, last_played_date = ?5, user_rating = ?6, likes = ?7
             WHERE user_id = ?8 AND item_id = ?9",
        )
        .bind(combined.position)
        .bind(combined.plays)
        .bind(i64::from(combined.favorite))
        .bind(i64::from(combined.played))
        .bind(combined.last_played.as_deref())
        .bind(combined.rating)
        .bind(combined.likes)
        .bind(&user)
        .bind(survivor)
        .execute(&mut *conn)
        .await?;
        sqlx::query("DELETE FROM user_item_data WHERE user_id = ?1 AND item_id = ?2")
            .bind(&user)
            .bind(other)
            .execute(&mut *conn)
            .await?;
    }
    for sql in [
        "UPDATE user_item_data SET item_id = ?1 WHERE item_id = ?2",
        "UPDATE pinned_episodes SET episode_id = ?1 WHERE episode_id = ?2",
        "UPDATE OR IGNORE collection_items SET item_id = ?1 WHERE item_id = ?2",
        "UPDATE OR IGNORE playlist_items SET item_id = ?1 WHERE item_id = ?2",
        "UPDATE activity_log SET item_id = ?1 WHERE item_id = ?2",
        "UPDATE sessions SET now_playing_item_id = ?1 WHERE now_playing_item_id = ?2",
        "UPDATE live_tv_recordings SET media_item_id = ?1 WHERE media_item_id = ?2",
    ] {
        sqlx::query(sql)
            .bind(survivor)
            .bind(other)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// `user`'s marks on item `item`. The row must exist.
async fn marks_on(
    conn: &mut SqliteConnection,
    user: &str,
    item: &str,
) -> Result<Marks, sqlx::Error> {
    let row: MarksRow = sqlx::query_as(
        "SELECT playback_position_ticks, play_count, is_favorite, played, last_played_date,
         user_rating, likes
         FROM user_item_data WHERE user_id = ?1 AND item_id = ?2",
    )
    .bind(user)
    .bind(item)
    .fetch_one(&mut *conn)
    .await?;
    let (position, plays, favorite, played, last_played, rating, likes) = row;
    Ok(Marks {
        position,
        plays,
        favorite: favorite != 0,
        played: played != 0,
        last_played,
        rating,
        likes,
    })
}

/// The row one user keeps when two copies merge. Flags are set if either row has them, and plays
/// add up. The later play gives the date and position, where a missing or unparseable date is the
/// oldest. On a tie the survivor's date is kept, or the other's if the survivor has none, and the
/// survivor's position is kept unless it is 0. The survivor's rating and likes win when set.
fn combine(survivor: &Marks, other: &Marks) -> Marks {
    let survivor_at = played_at(survivor.last_played.as_deref());
    let other_at = played_at(other.last_played.as_deref());
    let (last_played, position) = match other_at.cmp(&survivor_at) {
        Ordering::Greater => (other.last_played.clone(), other.position),
        Ordering::Less => (survivor.last_played.clone(), survivor.position),
        Ordering::Equal => (
            survivor
                .last_played
                .clone()
                .or_else(|| other.last_played.clone()),
            if survivor.position == 0 {
                other.position
            } else {
                survivor.position
            },
        ),
    };
    Marks {
        position,
        plays: survivor.plays + other.plays,
        favorite: survivor.favorite || other.favorite,
        played: survivor.played || other.played,
        last_played,
        rating: survivor.rating.or(other.rating),
        likes: survivor.likes.or(other.likes),
    }
}

/// The instant a stored play date stands for. A missing or unparseable date is `None`, which
/// sorts before every instant.
fn played_at(last_played: Option<&str>) -> Option<DateTime<FixedOffset>> {
    last_played.and_then(|date| DateTime::parse_from_rfc3339(date).ok())
}

/// The files of a group as strings, in order.
fn strings_of(files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .map(|file| file.to_string_lossy().into_owned())
        .collect()
}

/// The probe plan for a group's probeable files: the first is the primary, and each other file is
/// an alternate version. Empty when there are no files.
fn group_steps(files: &[String]) -> Vec<ProbeStep> {
    let mut steps = Vec::with_capacity(files.len());
    if let Some((primary, rest)) = files.split_first() {
        steps.push(ProbeStep::Primary(primary.clone()));
        steps.extend(rest.iter().map(|file| ProbeStep::Alternate(file.clone())));
    }
    steps
}

/// When an item was created, if its stored date parses. The repo writes RFC 3339 dates.
fn created_at(date_created: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(date_created)
        .ok()
        .map(|date| date.with_timezone(&Utc))
}

fn parse_id(id: &str) -> Result<Uuid, sqlx::Error> {
    Uuid::parse_str(id).map_err(|err| sqlx::Error::Decode(Box::new(err)))
}
