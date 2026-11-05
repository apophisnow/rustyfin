//! Rustyfin-side reads for the Jellyfin import preview (spec 2.5, 2.10): Rustyfin's users, and the
//! items a Jellyfin item can match. Read only; libraries come from `repo_library`.

use rustyfin_model::media::MediaType;
use sqlx::SqlitePool;
use uuid::Uuid;

/// A Rustyfin item an imported Jellyfin item can match (spec 2.10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportTarget {
    pub id: Uuid,
    pub media_type: MediaType,
    pub path: Option<String>,
    pub parent_id: Option<Uuid>,
    pub index_number: Option<i64>,
    pub parent_index_number: Option<i64>,
    pub tmdb_id: Option<i64>,
    pub tvdb_id: Option<i64>,
}

/// The `media_items` columns `import_targets` reads, in select order.
type TargetRow = (
    String,         // id
    String,         // media_type
    Option<String>, // path
    Option<String>, // parent_id
    Option<i64>,    // index_number
    Option<i64>,    // parent_index_number
    Option<i64>,    // tmdb_id
    Option<i64>,    // tvdb_id
);

/// Rustyfin's users as (id, name).
pub async fn user_names(pool: &SqlitePool) -> Result<Vec<(Uuid, String)>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (String, String)>("SELECT id, name FROM users")
        .fetch_all(pool)
        .await?;
    let mut users = Vec::with_capacity(rows.len());
    for (id, name) in rows {
        users.push((parse_id(&id)?, name));
    }
    Ok(users)
}

/// The Rustyfin items a Jellyfin item can match: movies, series, seasons, episodes and music.
pub async fn import_targets(pool: &SqlitePool) -> Result<Vec<ImportTarget>, sqlx::Error> {
    let rows = sqlx::query_as::<_, TargetRow>(
        "SELECT id, media_type, path, parent_id, index_number, parent_index_number, tmdb_id, tvdb_id \
         FROM media_items \
         WHERE media_type IN ('Movie', 'Series', 'Season', 'Episode', 'Audio', 'MusicAlbum')",
    )
    .fetch_all(pool)
    .await?;
    let mut targets = Vec::with_capacity(rows.len());
    for (id, media_type, path, parent_id, index_number, parent_index_number, tmdb_id, tvdb_id) in
        rows
    {
        let Ok(media_type) = media_type.parse::<MediaType>() else {
            continue;
        };
        targets.push(ImportTarget {
            id: parse_id(&id)?,
            media_type,
            path,
            parent_id: parent_id.as_deref().map(parse_id).transpose()?,
            index_number,
            parent_index_number,
            tmdb_id,
            tvdb_id,
        });
    }
    Ok(targets)
}

/// A stored id (hyphenated text) as a `Uuid`.
fn parse_id(text: &str) -> Result<Uuid, sqlx::Error> {
    Uuid::parse_str(text).map_err(|e| sqlx::Error::Decode(Box::new(e)))
}
