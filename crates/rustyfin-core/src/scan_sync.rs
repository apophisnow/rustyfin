//! Removal of items that a library scan no longer collects, the helper that deletes an item, and the
//! reconciliation of movie and episode version groups with the database.
//! All SQL that a scan uses to write items lives here.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rustyfin_model::library::{Library, LibraryContentType};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::naming::VersionGroup;
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

/// Deletes the items of `library` that the scan did not collect, and returns how many were deleted.
///
/// Step 1 covers the media types that the library's content type scans. An item is deleted only
/// when all of these hold: its path is under a library root; `fs::metadata` says it is a regular
/// file, or its path is under a folder the scan skipped as a symlink loop (such a path is never
/// collected again and may not resolve); its path is not in `collected`; and no folder in
/// `unreadable` contains it. So a missing or unreadable file is kept, unless it is under such a
/// loop. Step 2, for TV libraries only, then deletes the Seasons and then the Series that have no
/// children.
pub async fn remove_not_collected(
    pool: &SqlitePool,
    library: &Library,
    collected: &HashSet<String>,
    unreadable: &[PathBuf],
    loops: &[PathBuf],
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
            let in_loop = loops.iter().any(|dir| item.starts_with(dir));
            if !in_loop && !is_regular_file(item).await {
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
}

/// The fields that a group gives its item, by media type.
enum Wanted {
    /// A movie: its title and year.
    Movie { title: String, year: Option<i32> },
    /// An episode: the stem of its primary file, its numbers, and its season.
    Episode {
        name: String,
        index_number: Option<i32>,
        parent_index_number: Option<i32>,
        parent_id: Uuid,
    },
}

/// A group that [`reconcile_groups`] settles: its files, primary first, and the fields it gives.
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
    /// The group has no item yet. The scan creates one, then runs `steps` on it.
    New { steps: Vec<ProbeStep> },
}

/// Matches each movie version group with its item, merges the other owners of a group into the
/// survivor, updates the survivor's path and names, and says what to probe. The outcomes are in the
/// order of `groups`. Call this before the removal of items that are no longer collected.
pub async fn reconcile_movie_groups(
    pool: &SqlitePool,
    library_id: Uuid,
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
    reconcile_groups(pool, library_id, "Movie", &wanted).await
}

/// One episode's files, primary first, and the fields that its primary gives the item.
pub struct EpisodeGroup {
    pub files: Vec<PathBuf>,
    pub season_id: Uuid,
    pub index_number: Option<i32>,
    pub parent_index_number: Option<i32>,
}

/// Matches each episode group with its item, as [`reconcile_movie_groups`] does for movies. The
/// outcomes are in the order of `groups`. The name and sort name of an episode come from the stem
/// of its primary file. Call this before the removal of items that are no longer collected.
pub async fn reconcile_episode_groups(
    pool: &SqlitePool,
    library_id: Uuid,
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
    reconcile_groups(pool, library_id, "Episode", &wanted).await
}

/// Settles the groups of one media type in the library. The outcomes are in the order of `groups`.
///
/// Groups are handled in order of primary path, and the writes for one group run in one
/// transaction.
async fn reconcile_groups(
    pool: &SqlitePool,
    library_id: Uuid,
    media_type: &str,
    groups: &[WantedGroup],
) -> Result<Vec<GroupOutcome>, sqlx::Error> {
    let library = library_id.to_string();
    let items: Vec<ItemRow> = sqlx::query_as(
        "SELECT id, path, name, sort_name, production_year, runtime_ticks, date_created,
                index_number, parent_index_number, parent_id
         FROM media_items WHERE library_id = ?1 AND media_type = ?2",
    )
    .bind(library.as_str())
    .bind(media_type)
    .fetch_all(pool)
    .await?;
    let sources: Vec<SourceRow> = sqlx::query_as(
        "SELECT s.item_id, s.path, s.run_time_ticks
         FROM media_sources AS s
         JOIN media_items AS m ON m.id = s.item_id
         WHERE m.library_id = ?1 AND m.media_type = ?2
         ORDER BY s.rowid",
    )
    .bind(library.as_str())
    .bind(media_type)
    .fetch_all(pool)
    .await?;

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
    let group_paths: HashSet<&str> = groups
        .iter()
        .flat_map(|group| group.files.iter())
        .map(String::as_str)
        .collect();

    let mut order: Vec<usize> = (0..groups.len()).collect();
    order.sort_by_key(|&idx| groups[idx].files.first());
    let mut claimed: HashSet<&str> = HashSet::new();
    let mut outcomes: Vec<(usize, GroupOutcome)> = Vec::with_capacity(groups.len());
    for idx in order {
        let group = &groups[idx];
        let owners = owners_of(&group.files, &by_path, &by_source, &group_paths, &claimed);
        let outcome = match owners.split_first() {
            None => GroupOutcome::New {
                steps: group_steps(&group.files),
            },
            Some((&survivor, others)) => {
                claimed.extend(owners.iter().map(|owner| owner.id.as_str()));
                let survivor_sources = sources_of
                    .get(survivor.id.as_str())
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                settle_group(pool, group, survivor, others, survivor_sources).await?
            }
        };
        outcomes.push((idx, outcome));
    }
    outcomes.sort_by_key(|(idx, _)| *idx);
    Ok(outcomes.into_iter().map(|(_, outcome)| outcome).collect())
}

/// The items that own a group, sorted with the survivor first and without repeats. An owner is an
/// item whose path is one of `files`, or an unclaimed item whose path is in no group of this scan
/// and that has a source among `files`.
fn owners_of<'a>(
    files: &[String],
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
    let primary = files.first().map_or("", String::as_str);
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
/// survivor's fields that changed, and works out what to probe.
async fn settle_group(
    pool: &SqlitePool,
    group: &WantedGroup,
    survivor: &ItemRow,
    others: &[&ItemRow],
    sources: &[&SourceRow],
) -> Result<GroupOutcome, sqlx::Error> {
    // The owners come from the group's files, so there is a primary.
    let primary = group.files[0].as_str();
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

    // Steady state: nothing else owns the group, and the survivor's sources are the group's files,
    // in order, with the right duration. Otherwise the whole group is probed again. A primary with
    // no duration cannot be fixed by probing, so it counts as right.
    let source_paths_match = sources
        .iter()
        .map(|source| source.path.as_str())
        .eq(group.files.iter().map(String::as_str));
    let runtime_match = sources.first().is_some_and(|first| {
        first.run_time_ticks.is_none() || first.run_time_ticks == survivor.runtime_ticks
    });
    let steady =
        others.is_empty() && survivor.path == primary && source_paths_match && runtime_match;
    let steps = if steady {
        Vec::new()
    } else {
        group_steps(&group.files)
    };
    Ok(GroupOutcome::Existing {
        item_id: parse_id(&survivor.id)?,
        steps,
    })
}

/// Moves the rows that refer to item `other` over to item `survivor`. A row whose key the survivor
/// already has stays on `other`, and goes when `other` is deleted.
async fn merge_references(
    conn: &mut SqliteConnection,
    survivor: &str,
    other: &str,
) -> Result<(), sqlx::Error> {
    for sql in [
        "UPDATE OR IGNORE user_item_data SET item_id = ?1 WHERE item_id = ?2",
        "UPDATE pinned_episodes SET episode_id = ?1 WHERE episode_id = ?2",
        "UPDATE OR IGNORE collection_items SET item_id = ?1 WHERE item_id = ?2",
        "UPDATE OR IGNORE playlist_items SET item_id = ?1 WHERE item_id = ?2",
    ] {
        sqlx::query(sql)
            .bind(survivor)
            .bind(other)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// The files of a group as strings, in order.
fn strings_of(files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .map(|file| file.to_string_lossy().into_owned())
        .collect()
}

/// The full probe plan for a group: the primary file, then each other file as an alternate version.
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
