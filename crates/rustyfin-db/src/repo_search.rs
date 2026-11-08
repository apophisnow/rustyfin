use rustyfin_model::media::{MediaItem, MediaType};
use sqlx::SqlitePool;
use uuid::Uuid;

/// Normalize a search query for FTS5: escape special characters and add prefix wildcard
fn normalize_fts_query(query: &str) -> String {
    // Escape FTS5 special characters: * ? " \
    let escaped = query
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('*', "\\*")
        .replace('?', "\\?");
    // Add wildcard for prefix matching
    format!("{}*", escaped)
}

/// A library item that a hints search matched. The hint filters, the score and the paging run on
/// these.
pub struct HintCandidate {
    pub id: Uuid,
    pub name: String,
    /// The `media_type` as stored. It may be a name that is not a `BaseItemKind`.
    pub kind: String,
    /// The `tags` column split on `|`, each entry trimmed, empty entries dropped.
    pub tags: Vec<String>,
    pub production_year: Option<i32>,
    pub runtime_ticks: Option<i64>,
}

/// Hint candidates by FTS5. `?1` is the FTS query and `?2` the parent id, or NULL for every
/// library. A parent holds the items of that library and the descendants of that item, not the
/// item itself. `UNION` (not `UNION ALL`) ends a walk that runs into a cycle of parents.
const HINT_FTS_SQL: &str =
    "SELECT m.id, m.name, m.media_type, m.production_year, m.runtime_ticks, m.tags
     FROM media_items m
     JOIN media_items_fts f ON f.rowid = m.rowid
     WHERE media_items_fts MATCH ?1
       AND (?2 IS NULL OR m.library_id = ?2 OR (m.id <> ?2 AND m.id IN (
            WITH RECURSIVE below(id) AS (
                SELECT c.id FROM media_items c WHERE c.parent_id = ?2
                UNION
                SELECT c.id FROM media_items c JOIN below b ON c.parent_id = b.id
            )
            SELECT id FROM below)))";

/// The LIKE fallback of `HINT_FTS_SQL`. `?1` is the `query%` prefix pattern.
const HINT_LIKE_SQL: &str =
    "SELECT m.id, m.name, m.media_type, m.production_year, m.runtime_ticks, m.tags
     FROM media_items m
     WHERE m.name LIKE ?1
       AND (?2 IS NULL OR m.library_id = ?2 OR (m.id <> ?2 AND m.id IN (
            WITH RECURSIVE below(id) AS (
                SELECT c.id FROM media_items c WHERE c.parent_id = ?2
                UNION
                SELECT c.id FROM media_items c JOIN below b ON c.parent_id = b.id
            )
            SELECT id FROM below)))";

type HintRow = (
    String,
    String,
    String,
    Option<i32>,
    Option<i64>,
    Option<String>,
);

/// Every library item that a hints search for `term` (trimmed) matches inside `parent`. The match
/// is FTS5 over the names, overviews and genres, with the name prefix as the fallback when FTS5
/// rejects the query. The result is neither ordered nor limited.
pub async fn hint_candidates(
    pool: &SqlitePool,
    term: &str,
    parent: Option<Uuid>,
) -> Result<Vec<HintCandidate>, sqlx::Error> {
    let scope = parent.map(|id| id.to_string());
    let fts_query = normalize_fts_query(term);
    let rows = sqlx::query_as::<_, HintRow>(HINT_FTS_SQL)
        .bind(&fts_query)
        .bind(scope.clone())
        .fetch_all(pool)
        .await;

    let rows = match rows {
        Ok(rows) => rows,
        Err(_) => {
            // Using `query%` instead of `%query%` enables B-tree index scan on idx_media_items_name
            let like_prefix = format!("{term}%");
            sqlx::query_as::<_, HintRow>(HINT_LIKE_SQL)
                .bind(&like_prefix)
                .bind(scope)
                .fetch_all(pool)
                .await?
        }
    };

    Ok(rows
        .into_iter()
        .filter_map(|(id, name, kind, production_year, runtime_ticks, tags)| {
            Some(HintCandidate {
                id: Uuid::parse_str(&id).ok()?,
                name,
                kind,
                tags: split_tags(tags.as_deref()),
                production_year,
                runtime_ticks,
            })
        })
        .collect())
}

/// The entries of a `tags` value: split on `|`, trimmed, with the empty ones dropped.
fn split_tags(tags: Option<&str>) -> Vec<String> {
    tags.unwrap_or_default()
        .split('|')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .collect()
}

/// Full search returning MediaItem results
/// Uses FTS5 for full-text search across name, overview, and genres,
/// with LIKE prefix matching as fallback.
pub async fn search_items(
    pool: &SqlitePool,
    query: &str,
    item_types: &[MediaType],
    limit: i64,
) -> Result<Vec<MediaItem>, sqlx::Error> {
    // Build media type filter
    let type_filters: Vec<&'static str> = item_types.iter().map(|t| t.item_kind()).collect();

    // Try FTS5 first for full-text search
    let fts_query = normalize_fts_query(query);
    let rows = if type_filters.is_empty() {
        // Search all types via FTS5
        sqlx::query_as::<_, super::repo_media::MediaItemRow>(
            "SELECT m.id, m.name, m.sort_name, m.overview, m.media_type, m.path, m.parent_id, m.library_id, \
             m.production_year, m.community_rating, m.official_rating, m.genres, m.date_created, \
             m.premiere_date, m.runtime_ticks, m.index_number, m.parent_index_number, m.tmdb_id, m.tvdb_id, \
             m.tags, m.status, m.original_language, m.tagline, m.production_countries, m.budget, m.revenue, \
             m.homepage, m.end_date, m.vote_count, m.popularity \
             FROM media_items m \
             JOIN media_items_fts f ON f.rowid = m.rowid \
             WHERE media_items_fts MATCH ?1 \
             LIMIT ?2",
        )
        .bind(&fts_query)
        .bind(limit)
        .fetch_all(pool)
        .await
    } else {
        // Search with media type filter via FTS5. All placeholders are bare `?`, bound in
        // order: mixing `?N` with `?` makes SQLite reuse ?2 for LIMIT.
        let placeholders: Vec<&str> = type_filters.iter().map(|_| "?").collect();
        let sql = format!(
            "SELECT m.id, m.name, m.sort_name, m.overview, m.media_type, m.path, m.parent_id, m.library_id, \
             m.production_year, m.community_rating, m.official_rating, m.genres, m.date_created, \
             m.premiere_date, m.runtime_ticks, m.index_number, m.parent_index_number, m.tmdb_id, m.tvdb_id, \
             m.tags, m.status, m.original_language, m.tagline, m.production_countries, m.budget, m.revenue, \
             m.homepage, m.end_date, m.vote_count, m.popularity \
             FROM media_items m \
             JOIN media_items_fts f ON f.rowid = m.rowid \
             WHERE media_items_fts MATCH ? AND m.media_type IN ({}) \
             LIMIT ?",
            placeholders.join(",")
        );
        let mut q = sqlx::query_as::<_, super::repo_media::MediaItemRow>(sqlx::AssertSqlSafe(sql));
        q = q.bind(&fts_query);
        for t in &type_filters {
            q = q.bind(t);
        }
        q = q.bind(limit);
        q.fetch_all(pool).await
    };

    // If FTS5 succeeds, return results
    match rows {
        Ok(rows) => Ok(rows.into_iter().map(Into::into).collect()),
        Err(_) => {
            // Fallback to LIKE search with prefix matching for index utilization
            // Using `query%` instead of `%query%` enables B-tree index scan
            let like_prefix = format!("{}%", query);

            if type_filters.is_empty() {
                let rows = sqlx::query_as::<_, super::repo_media::MediaItemRow>(
                    "SELECT id, name, sort_name, overview, media_type, path, parent_id, library_id, \
                     production_year, community_rating, official_rating, genres, date_created, \
                     premiere_date, runtime_ticks, index_number, parent_index_number, tmdb_id, tvdb_id, \
                     tags, status, original_language, tagline, production_countries, budget, revenue, \
                     homepage, end_date, vote_count, popularity \
                     FROM media_items WHERE name LIKE ?1 LIMIT ?2",
                )
                .bind(&like_prefix)
                .bind(limit)
                .fetch_all(pool)
                .await?;
                Ok(rows.into_iter().map(Into::into).collect())
            } else {
                let placeholders: Vec<&str> = type_filters.iter().map(|_| "?").collect();
                let sql = format!(
                    "SELECT id, name, sort_name, overview, media_type, path, parent_id, library_id, \
                     production_year, community_rating, official_rating, genres, date_created, \
                     premiere_date, runtime_ticks, index_number, parent_index_number, tmdb_id, tvdb_id, \
                     tags, status, original_language, tagline, production_countries, budget, revenue, \
                     homepage, end_date, vote_count, popularity \
                     FROM media_items WHERE name LIKE ? AND media_type IN ({}) LIMIT ?",
                    placeholders.join(",")
                );
                let mut q =
                    sqlx::query_as::<_, super::repo_media::MediaItemRow>(sqlx::AssertSqlSafe(sql));
                q = q.bind(&like_prefix);
                for t in &type_filters {
                    q = q.bind(t);
                }
                q = q.bind(limit);
                let rows = q.fetch_all(pool).await?;
                Ok(rows.into_iter().map(Into::into).collect())
            }
        }
    }
}
