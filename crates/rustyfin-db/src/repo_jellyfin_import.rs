//! Rustyfin-side reads and writes for the Jellyfin import (spec 2.5, 2.10, 2.31): Rustyfin's users,
//! the items a Jellyfin item can match, and the watch data and track choices of the import.
//! Libraries come from `repo_library`.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
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

/// One user's watch data for one item (spec 2.31): a `user_item_data` row.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserDataRow {
    pub played: bool,
    pub play_count: i64,
    pub position_ticks: i64,
    pub last_played: Option<DateTime<Utc>>,
    pub is_favorite: bool,
    pub user_rating: Option<i64>,
    pub likes: Option<bool>,
    /// The remembered audio stream index, in Rustyfin's numbering.
    pub audio: Option<i64>,
    /// The remembered subtitle stream index, in Rustyfin's numbering; -1 means subtitles off.
    pub subtitle: Option<i64>,
}

/// A media stream of a Rustyfin media source (spec 2.31): a `media_streams` row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamRow {
    pub index: i64,
    pub codec_type: String,
    pub codec: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub is_external: bool,
    pub path: Option<String>,
    pub is_default: bool,
    pub is_forced: bool,
}

/// A media source of an item, with its streams in `stream_index` order (spec 2.31).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceStreams {
    pub path: String,
    pub streams: Vec<StreamRow>,
}

/// `user_item_data`: item_id, played, play_count, playback_position_ticks, last_played_date,
/// is_favorite, user_rating, likes, audio_stream_index, subtitle_stream_index.
type DataDbRow = (
    String,
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

/// `media_streams`: stream_index, codec_type, codec, language, title, is_external, path, is_default,
/// is_forced.
type StreamDbRow = (
    i64,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
    Option<String>,
    bool,
    bool,
);

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

/// The user's watch data by item (spec 2.31). A stored date that doesn't parse reads as none; a row
/// whose item id doesn't parse is skipped.
pub async fn user_data_rows(
    pool: &SqlitePool,
    user: Uuid,
) -> Result<HashMap<Uuid, UserDataRow>, sqlx::Error> {
    let rows = sqlx::query_as::<_, DataDbRow>(
        "SELECT item_id, played, play_count, playback_position_ticks, last_played_date, \
         is_favorite, user_rating, likes, audio_stream_index, subtitle_stream_index \
         FROM user_item_data WHERE user_id = ?",
    )
    .bind(user.to_string())
    .fetch_all(pool)
    .await?;
    let mut data = HashMap::with_capacity(rows.len());
    for (
        item,
        played,
        play_count,
        position_ticks,
        last_played,
        is_favorite,
        user_rating,
        likes,
        audio,
        subtitle,
    ) in rows
    {
        let Ok(item) = Uuid::parse_str(&item) else {
            continue;
        };
        data.insert(
            item,
            UserDataRow {
                played,
                play_count,
                position_ticks,
                last_played: last_played.as_deref().and_then(parse_date),
                is_favorite,
                user_rating,
                likes,
                audio,
                subtitle,
            },
        );
    }
    Ok(data)
}

/// Writes the user's rows in one transaction (spec 2.31): each row is inserted, or every value
/// column of its existing row is replaced.
pub async fn write_user_data(
    pool: &SqlitePool,
    user: Uuid,
    rows: &[(Uuid, UserDataRow)],
) -> Result<(), sqlx::Error> {
    let user_id = user.to_string();
    let mut tx = pool.begin().await?;
    for (item, row) in rows {
        sqlx::query(
            "INSERT INTO user_item_data (user_id, item_id, played, play_count, \
             playback_position_ticks, last_played_date, is_favorite, user_rating, likes, \
             audio_stream_index, subtitle_stream_index) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, item_id) DO UPDATE SET \
             played = excluded.played, \
             play_count = excluded.play_count, \
             playback_position_ticks = excluded.playback_position_ticks, \
             last_played_date = excluded.last_played_date, \
             is_favorite = excluded.is_favorite, \
             user_rating = excluded.user_rating, \
             likes = excluded.likes, \
             audio_stream_index = excluded.audio_stream_index, \
             subtitle_stream_index = excluded.subtitle_stream_index",
        )
        .bind(&user_id)
        .bind(item.to_string())
        .bind(row.played)
        .bind(row.play_count)
        .bind(row.position_ticks)
        .bind(row.last_played.map(|date| date.to_rfc3339()))
        .bind(row.is_favorite)
        .bind(row.user_rating)
        .bind(row.likes)
        .bind(row.audio)
        .bind(row.subtitle)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// The item's media sources by `created_at` then `id`, each with its streams by `stream_index`
/// (spec 2.31).
pub async fn item_sources(
    pool: &SqlitePool,
    item: Uuid,
) -> Result<Vec<SourceStreams>, sqlx::Error> {
    let sources = sqlx::query_as::<_, (String, String)>(
        "SELECT id, path FROM media_sources WHERE item_id = ? ORDER BY created_at, id",
    )
    .bind(item.to_string())
    .fetch_all(pool)
    .await?;
    let mut result = Vec::with_capacity(sources.len());
    for (source_id, source_path) in sources {
        let rows = sqlx::query_as::<_, StreamDbRow>(
            "SELECT stream_index, codec_type, codec, language, title, is_external, path, \
             is_default, is_forced FROM media_streams WHERE source_id = ? ORDER BY stream_index",
        )
        .bind(&source_id)
        .fetch_all(pool)
        .await?;
        let streams = rows
            .into_iter()
            .map(
                |(
                    index,
                    codec_type,
                    codec,
                    language,
                    title,
                    is_external,
                    path,
                    is_default,
                    is_forced,
                )| {
                    StreamRow {
                        index,
                        codec_type,
                        codec,
                        language,
                        title,
                        is_external,
                        path,
                        is_default,
                        is_forced,
                    }
                },
            )
            .collect();
        result.push(SourceStreams {
            path: source_path,
            streams,
        });
    }
    Ok(result)
}

/// A stored id (hyphenated text) as a `Uuid`.
fn parse_id(text: &str) -> Result<Uuid, sqlx::Error> {
    Uuid::parse_str(text).map_err(|e| sqlx::Error::Decode(Box::new(e)))
}

/// A stored date (RFC 3339, as Rustyfin writes it) in UTC; text that doesn't parse is None.
fn parse_date(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|date| date.with_timezone(&Utc))
}
