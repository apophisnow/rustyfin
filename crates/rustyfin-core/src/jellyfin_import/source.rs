//! Reading a Jellyfin data folder (spec 2.1-2.4, 2.12-2.13). Each database is opened read-only and
//! read inside one transaction, which is rolled back.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use rustyfin_model::media::MediaType;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Acquire, ConnectOptions, SqliteConnection};
use uuid::Uuid;

use super::{ImportError, Layout, LayoutChoice};

/// Jellyfin's database, in the data folder.
const JELLYFIN_DB: &str = "jellyfin.db";
/// The old layout's database, and the name an upgrade gives it.
const LIBRARY_DB: &str = "library.db";
const LIBRARY_DB_OLD: &str = "library.db.old";

/// Where Jellyfin keeps its data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    /// The folder holding `jellyfin.db` (Jellyfin's `DataPath`, `%AppDataPath%`).
    pub data_path: PathBuf,
    /// Its parent (Jellyfin's `ProgramDataPath`, holding `root/` and `metadata/`).
    pub program_data_path: PathBuf,
}

/// Finds `jellyfin.db` at `<source>`, `<source>/data` or `<source>/data/data` (spec 2.1). The
/// paths are used as given; nothing is canonicalised.
pub fn locate(source: &Path) -> Result<SourceLocation, ImportError> {
    if source.as_os_str().is_empty() || !source.is_absolute() {
        return Err(ImportError::BadSource(
            "SourcePath must be an absolute folder".to_string(),
        ));
    }
    let candidates = [
        source.join(JELLYFIN_DB),
        source.join("data").join(JELLYFIN_DB),
        source.join("data").join("data").join(JELLYFIN_DB),
    ];
    let Some(found) = candidates.iter().find(|path| path.is_file()) else {
        return Err(ImportError::BadSource(format!(
            "no jellyfin.db at {}, {} or {}",
            candidates[0].display(),
            candidates[1].display(),
            candidates[2].display(),
        )));
    };
    let data_path = found.parent().unwrap_or(source).to_path_buf();
    let program_data_path = data_path
        .parent()
        .map_or_else(|| data_path.clone(), Path::to_path_buf);
    Ok(SourceLocation {
        data_path,
        program_data_path,
    })
}

/// Parses Jellyfin's dates (spec 2.13): RFC 3339, or `YYYY-MM-DD HH:MM:SS[.f]` with up to seven
/// fraction digits and an optional `Z`. Both are UTC; anything else is no date.
pub fn parse_jellyfin_date(text: &str) -> Option<DateTime<Utc>> {
    if let Ok(dated) = DateTime::parse_from_rfc3339(text) {
        return Some(dated.with_timezone(&Utc));
    }
    let naive = text.strip_suffix('Z').unwrap_or(text);
    let fraction_ok = naive.split_once('.').is_none_or(|(_, fraction)| {
        (1..=7).contains(&fraction.len()) && fraction.bytes().all(|b| b.is_ascii_digit())
    });
    if !fraction_ok {
        return None;
    }
    NaiveDateTime::parse_from_str(naive, "%Y-%m-%d %H:%M:%S%.f")
        .ok()
        .map(|naive| Utc.from_utc_datetime(&naive))
}

/// A GUID stored as text, in any case (spec 2.3a).
pub fn guid_from_text(text: &str) -> Option<Uuid> {
    Uuid::parse_str(text).ok()
}

/// A GUID stored as a 16-byte BLOB in .NET byte order (spec 2.3a).
pub fn guid_from_blob(bytes: &[u8]) -> Option<Uuid> {
    let array: [u8; 16] = bytes.try_into().ok()?;
    Some(Uuid::from_bytes_le(array))
}

/// A GUID column: 16 bytes are a .NET-order BLOB, anything else is UTF-8 text (spec 2.3a).
fn guid_column(bytes: Option<&[u8]>) -> Option<Uuid> {
    let bytes = bytes?;
    if bytes.len() == 16 {
        guid_from_blob(bytes)
    } else {
        guid_from_text(std::str::from_utf8(bytes).ok()?)
    }
}

/// The provider ids the importer uses (spec 2.3a).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProviderIds {
    pub tmdb: Option<i64>,
    pub tvdb: Option<i64>,
    pub imdb: Option<String>,
}

impl ProviderIds {
    /// Records `value` for `provider` ("Tmdb", "Tvdb", "Imdb", any case); bad values and other
    /// providers are ignored.
    pub fn set(&mut self, provider: &str, value: &str) {
        match provider.to_ascii_lowercase().as_str() {
            "tmdb" => self.tmdb = whole_number(value).or(self.tmdb),
            "tvdb" => self.tvdb = whole_number(value).or(self.tvdb),
            "imdb" if is_imdb(value) => self.imdb = Some(value.to_string()),
            _ => {}
        }
    }
}

/// Digits only, as a number (`Tmdb=1`); anything else is `None`.
fn whole_number(text: &str) -> Option<i64> {
    if text.bytes().all(|b| b.is_ascii_digit()) {
        text.parse().ok()
    } else {
        None
    }
}

/// An IMDb id: `tt` and one or more digits.
fn is_imdb(text: &str) -> bool {
    text.strip_prefix("tt")
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

/// Parses the old layout's `ProviderIds` text (`Tmdb=1|Imdb=tt2|Tvdb=3`).
pub fn parse_provider_ids(text: &str) -> ProviderIds {
    let mut ids = ProviderIds::default();
    for pair in text.split('|') {
        if let Some((provider, value)) = pair.split_once('=') {
            ids.set(provider, value);
        }
    }
    ids
}

/// A Jellyfin user (spec 2.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JfUser {
    pub id: Uuid,
    pub name: String,
    /// `Users.InternalId`, which the old layout's `UserDatas.userId` refers to.
    pub internal_id: i64,
    pub is_admin: bool,
}

/// A Jellyfin item of one of the spec 2.9 types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JfItem {
    pub id: Uuid,
    pub media_type: MediaType,
    /// The last part of the .NET type name, as the report shows it (`Movie`, `Video`, ...).
    pub type_name: String,
    pub name: Option<String>,
    pub path: Option<String>,
    pub parent_id: Option<Uuid>,
    pub series_id: Option<Uuid>,
    pub season_id: Option<Uuid>,
    pub index_number: Option<i64>,
    pub parent_index_number: Option<i64>,
    pub providers: ProviderIds,
}

/// One user's watch data for one item (spec 2.28): a `UserData` row, or a `UserDatas` row of the
/// old layout (which has no `likes`).
#[derive(Debug, Clone, PartialEq)]
pub struct JfUserData {
    pub item: Uuid,
    pub user: Uuid,
    pub played: bool,
    pub play_count: i64,
    pub position_ticks: i64,
    pub last_played: Option<DateTime<Utc>>,
    pub is_favorite: bool,
    /// Jellyfin's rating, 0 to 10 with decimals.
    pub rating: Option<f64>,
    pub likes: Option<bool>,
    /// The chosen audio stream index, in Jellyfin's numbering.
    pub audio: Option<i64>,
    /// The chosen subtitle stream index, in Jellyfin's numbering; -1 means subtitles off.
    pub subtitle: Option<i64>,
}

/// The kind of a media stream (spec 2.28).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Audio,
    Video,
    Subtitle,
    Other,
}

/// One media stream of a Jellyfin item (spec 2.28).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JfStream {
    pub item: Uuid,
    pub index: i64,
    pub kind: StreamKind,
    pub codec: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub is_external: bool,
    pub path: Option<String>,
    pub is_default: bool,
    pub is_forced: bool,
}

/// What the preview reads from a source.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub layout: Layout,
    pub users: Vec<JfUser>,
    pub items: Vec<JfItem>,
    /// Every user-data row (spec 2.28).
    pub user_data: Vec<JfUserData>,
    /// The media streams of the items that have a track choice (spec 2.28).
    pub streams: Vec<JfStream>,
}

/// A Jellyfin library folder `root/default/<name>/` (spec 2.8): the type its `<type>.collection`
/// file names (if any) and the content of each `*.mblink` (a Jellyfin media path), in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryFolder {
    pub name: String,
    pub collection_type: Option<String>,
    pub mblinks: Vec<String>,
}

// Column rows of the queries below; the comments name the columns in select order.

/// `Users`: Id, Username, InternalId.
type UserDbRow = (Option<Vec<u8>>, String, i64);
/// `BaseItems`: Id, Type, Name, Path, ParentId, SeriesId, SeasonId, IndexNumber, ParentIndexNumber.
type ItemDbRow = (
    Option<Vec<u8>>,
    String,
    Option<String>,
    Option<String>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<i64>,
    Option<i64>,
);
/// `BaseItemProviders`: ItemId, ProviderId, ProviderValue.
type ProviderDbRow = (Option<Vec<u8>>, String, String);
/// `UserData`: ItemId, UserId, Played, PlayCount, PlaybackPositionTicks, LastPlayedDate, IsFavorite,
/// Rating, Likes, AudioStreamIndex, SubtitleStreamIndex.
type UserDataDbRow = (
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<bool>,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<bool>,
    Option<f64>,
    Option<bool>,
    Option<i64>,
    Option<i64>,
);
/// `TypedBaseItems`: guid, type, Name, Path, ParentId, SeriesId, SeasonId, IndexNumber,
/// ParentIndexNumber, UserDataKey, ProviderIds.
type LibraryItemDbRow = (
    Option<Vec<u8>>,
    String,
    Option<String>,
    Option<String>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<String>,
);
/// `UserDatas`: key, userId, played, playCount, playbackPositionTicks, lastPlayedDate, isFavorite,
/// rating, AudioStreamIndex, SubtitleStreamIndex.
type LibraryDataDbRow = (
    String,
    i64,
    Option<bool>,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<bool>,
    Option<f64>,
    Option<i64>,
    Option<i64>,
);
/// `MediaStreamInfos` (new) or `mediastreams` (old): ItemId, StreamIndex, StreamType, Codec,
/// Language, Title, IsExternal, Path, IsDefault, IsForced. `Kind` is the layout's `StreamType` type.
type StreamDbRow<Kind> = (
    Option<Vec<u8>>,
    Option<i64>,
    Option<Kind>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<bool>,
    Option<String>,
    Option<bool>,
    Option<bool>,
);
/// The old layout's `library.db` rows: `TypedBaseItems`, `UserDatas` and `mediastreams`.
type LibraryRows = (
    Vec<LibraryItemDbRow>,
    Vec<LibraryDataDbRow>,
    Vec<StreamDbRow<String>>,
);
/// Items, user-data rows and streams of one layout.
type ItemRows = (Vec<JfItem>, Vec<JfUserData>, Vec<JfStream>);

/// Reads the source's users and importable items (spec 2.2, 2.3). The new layout comes from
/// `jellyfin.db`; the old one from `library.db`, else `library.db.old`. Users always come from
/// `jellyfin.db`.
pub async fn read_snapshot(
    location: &SourceLocation,
    choice: LayoutChoice,
) -> Result<Snapshot, ImportError> {
    let file = location.data_path.join(JELLYFIN_DB);
    let mut conn = open_read_only(&file).await?;
    let mut tx = conn.begin().await.map_err(|e| unreadable(&file, &e))?;
    let db = read_jellyfin_db(&mut tx, choice)
        .await
        .map_err(|e| unreadable(&file, &e))?;
    tx.rollback().await.map_err(|e| unreadable(&file, &e))?;

    let (items, user_data, streams) = match db.layout {
        Layout::New => (db.items, db.user_data, db.streams),
        Layout::Old => read_old_layout(&location.data_path, &db.users).await?,
    };
    Ok(Snapshot {
        layout: db.layout,
        users: db.users,
        items,
        user_data,
        streams,
    })
}

/// What `jellyfin.db` gives: its users, the layout the request resolves to, and for the new layout
/// its items, user-data rows and streams.
struct JellyfinDb {
    layout: Layout,
    users: Vec<JfUser>,
    items: Vec<JfItem>,
    user_data: Vec<JfUserData>,
    streams: Vec<JfStream>,
}

async fn read_jellyfin_db(
    conn: &mut SqliteConnection,
    choice: LayoutChoice,
) -> Result<JellyfinDb, sqlx::Error> {
    let user_rows: Vec<UserDbRow> = sqlx::query_as("SELECT Id, Username, InternalId FROM Users")
        .fetch_all(&mut *conn)
        .await?;
    let admin_rows: Vec<(Option<Vec<u8>>,)> =
        sqlx::query_as("SELECT UserId FROM Permissions WHERE Kind = 0 AND Value = 1")
            .fetch_all(&mut *conn)
            .await?;
    let admins: HashSet<Uuid> = admin_rows
        .iter()
        .filter_map(|(id,)| guid_column(id.as_deref()))
        .collect();
    let users: Vec<JfUser> = user_rows
        .into_iter()
        .filter_map(|(id, name, internal_id)| {
            let id = guid_column(id.as_deref())?;
            Some(JfUser {
                id,
                name,
                internal_id,
                is_admin: admins.contains(&id),
            })
        })
        .collect();

    let (base_tables,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'BaseItems'",
    )
    .fetch_one(&mut *conn)
    .await?;
    let layout = match choice {
        LayoutChoice::Auto if base_tables > 0 => Layout::New,
        LayoutChoice::Auto | LayoutChoice::Old => Layout::Old,
        LayoutChoice::New => Layout::New,
    };
    let (items, user_data, streams) = match layout {
        Layout::New => read_new_layout(conn).await?,
        Layout::Old => (Vec::new(), Vec::new(), Vec::new()),
    };
    Ok(JellyfinDb {
        layout,
        users,
        items,
        user_data,
        streams,
    })
}

/// The new layout's items, provider ids, user-data rows and streams (spec 2.3a, 2.12, 2.28).
async fn read_new_layout(conn: &mut SqliteConnection) -> Result<ItemRows, sqlx::Error> {
    let provider_rows: Vec<ProviderDbRow> =
        sqlx::query_as("SELECT ItemId, ProviderId, ProviderValue FROM BaseItemProviders")
            .fetch_all(&mut *conn)
            .await?;
    let mut providers: HashMap<Uuid, ProviderIds> = HashMap::new();
    for (item, provider, value) in provider_rows {
        if let Some(item) = guid_column(item.as_deref()) {
            providers.entry(item).or_default().set(&provider, &value);
        }
    }

    let item_rows: Vec<ItemDbRow> = sqlx::query_as(
        "SELECT Id, Type, Name, Path, ParentId, SeriesId, SeasonId, IndexNumber, \
         ParentIndexNumber FROM BaseItems",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut items = Vec::new();
    for (id, type_name, name, path, parent, series, season, index, parent_index) in item_rows {
        let Some((media_type, type_name)) = importable(&type_name) else {
            continue;
        };
        let Some(id) = guid_column(id.as_deref()) else {
            continue;
        };
        items.push(JfItem {
            id,
            media_type,
            type_name,
            name,
            path,
            parent_id: guid_column(parent.as_deref()),
            series_id: guid_column(series.as_deref()),
            season_id: guid_column(season.as_deref()),
            index_number: index,
            parent_index_number: parent_index,
            providers: providers.remove(&id).unwrap_or_default(),
        });
    }

    let data_rows: Vec<UserDataDbRow> = sqlx::query_as(
        "SELECT ItemId, UserId, Played, PlayCount, PlaybackPositionTicks, LastPlayedDate, \
         IsFavorite, Rating, Likes, AudioStreamIndex, SubtitleStreamIndex FROM UserData",
    )
    .fetch_all(&mut *conn)
    .await?;
    let user_data: Vec<JfUserData> = data_rows
        .into_iter()
        .filter_map(
            |(
                item,
                user,
                played,
                plays,
                position,
                last,
                favorite,
                rating,
                likes,
                audio,
                subtitle,
            )| {
                Some(JfUserData {
                    item: guid_column(item.as_deref())?,
                    user: guid_column(user.as_deref())?,
                    played: played.unwrap_or(false),
                    play_count: plays.unwrap_or(0),
                    position_ticks: position.unwrap_or(0),
                    last_played: last.as_deref().and_then(parse_jellyfin_date),
                    is_favorite: favorite.unwrap_or(false),
                    rating,
                    likes,
                    audio,
                    subtitle,
                })
            },
        )
        .collect();
    let stream_rows: Vec<StreamDbRow<i64>> = sqlx::query_as(
        "SELECT ItemId, StreamIndex, StreamType, Codec, Language, Title, IsExternal, Path, \
         IsDefault, IsForced FROM MediaStreamInfos WHERE ItemId IN (SELECT ItemId FROM UserData \
         WHERE AudioStreamIndex IS NOT NULL OR SubtitleStreamIndex IS NOT NULL)",
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok((
        items,
        user_data,
        streams_of(stream_rows, stream_kind_number),
    ))
}

/// The old layout's items, user-data rows and streams, from `library.db` (else
/// `library.db.old`). A row reaches every importable item that has its `key` (`UserDataKey`).
async fn read_old_layout(data_path: &Path, users: &[JfUser]) -> Result<ItemRows, ImportError> {
    let file = library_file(data_path)?;
    let mut conn = open_read_only(&file).await?;
    let mut tx = conn.begin().await.map_err(|e| unreadable(&file, &e))?;
    let (item_rows, data_rows, stream_rows) = read_library_db(&mut tx)
        .await
        .map_err(|e| unreadable(&file, &e))?;
    tx.rollback().await.map_err(|e| unreadable(&file, &e))?;

    let mut items = Vec::new();
    let mut keyed: HashMap<String, Vec<Uuid>> = HashMap::new();
    for (
        guid,
        type_name,
        name,
        path,
        parent,
        series,
        season,
        index,
        parent_index,
        key,
        provider_ids,
    ) in item_rows
    {
        let Some((media_type, type_name)) = importable(&type_name) else {
            continue;
        };
        let Some(id) = guid_column(guid.as_deref()) else {
            continue;
        };
        if let Some(key) = key {
            keyed.entry(key).or_default().push(id);
        }
        items.push(JfItem {
            id,
            media_type,
            type_name,
            name,
            path,
            parent_id: guid_column(parent.as_deref()),
            series_id: guid_column(series.as_deref()),
            season_id: guid_column(season.as_deref()),
            index_number: index,
            parent_index_number: parent_index,
            providers: parse_provider_ids(provider_ids.as_deref().unwrap_or_default()),
        });
    }

    let internal_ids: HashMap<i64, Uuid> = users
        .iter()
        .map(|user| (user.internal_id, user.id))
        .collect();
    let mut user_data = Vec::new();
    for (key, internal_id, played, plays, position, last, favorite, rating, audio, subtitle) in
        data_rows
    {
        let Some(&user) = internal_ids.get(&internal_id) else {
            continue;
        };
        for &item in keyed.get(&key).into_iter().flatten() {
            user_data.push(JfUserData {
                item,
                user,
                played: played.unwrap_or(false),
                play_count: plays.unwrap_or(0),
                position_ticks: position.unwrap_or(0),
                last_played: last.as_deref().and_then(parse_jellyfin_date),
                is_favorite: favorite.unwrap_or(false),
                rating,
                likes: None,
                audio,
                subtitle,
            });
        }
    }
    Ok((items, user_data, streams_of(stream_rows, stream_kind_text)))
}

async fn read_library_db(conn: &mut SqliteConnection) -> Result<LibraryRows, sqlx::Error> {
    let items: Vec<LibraryItemDbRow> = sqlx::query_as(
        "SELECT guid, type, Name, Path, ParentId, SeriesId, SeasonId, IndexNumber, \
         ParentIndexNumber, UserDataKey, ProviderIds FROM TypedBaseItems",
    )
    .fetch_all(&mut *conn)
    .await?;
    let data: Vec<LibraryDataDbRow> = sqlx::query_as(
        "SELECT key, userId, played, playCount, playbackPositionTicks, lastPlayedDate, isFavorite, \
         rating, AudioStreamIndex, SubtitleStreamIndex FROM UserDatas",
    )
    .fetch_all(&mut *conn)
    .await?;
    let streams: Vec<StreamDbRow<String>> = sqlx::query_as(
        "SELECT ItemId, StreamIndex, StreamType, Codec, Language, Title, IsExternal, Path, \
         IsDefault, IsForced FROM mediastreams WHERE ItemId IN (SELECT guid FROM TypedBaseItems \
         WHERE UserDataKey IN (SELECT key FROM UserDatas WHERE AudioStreamIndex IS NOT NULL \
         OR SubtitleStreamIndex IS NOT NULL))",
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok((items, data, streams))
}

/// `StreamType` of the new layout: an INTEGER, 0 Audio, 1 Video, 2 Subtitle, else Other.
fn stream_kind_number(value: Option<i64>) -> StreamKind {
    match value {
        Some(0) => StreamKind::Audio,
        Some(1) => StreamKind::Video,
        Some(2) => StreamKind::Subtitle,
        _ => StreamKind::Other,
    }
}

/// `StreamType` of the old layout: TEXT `Audio`, `Video` or `Subtitle`, else Other.
fn stream_kind_text(value: Option<String>) -> StreamKind {
    match value.as_deref() {
        Some("Audio") => StreamKind::Audio,
        Some("Video") => StreamKind::Video,
        Some("Subtitle") => StreamKind::Subtitle,
        _ => StreamKind::Other,
    }
}

/// Turns stream rows into `JfStream`s (spec 2.28). A row without a valid item id or index is
/// skipped; NULL flags read as false.
fn streams_of<Kind>(
    rows: Vec<StreamDbRow<Kind>>,
    kind_of: fn(Option<Kind>) -> StreamKind,
) -> Vec<JfStream> {
    rows.into_iter()
        .filter_map(
            |(
                item,
                index,
                kind,
                codec,
                language,
                title,
                is_external,
                path,
                is_default,
                is_forced,
            )| {
                Some(JfStream {
                    item: guid_column(item.as_deref())?,
                    index: index?,
                    kind: kind_of(kind),
                    codec,
                    language,
                    title,
                    is_external: is_external.unwrap_or(false),
                    path,
                    is_default: is_default.unwrap_or(false),
                    is_forced: is_forced.unwrap_or(false),
                })
            },
        )
        .collect()
}

/// The old layout's database: `library.db`, else the `library.db.old` an upgrade leaves.
fn library_file(data_path: &Path) -> Result<PathBuf, ImportError> {
    let current = data_path.join(LIBRARY_DB);
    if current.is_file() {
        return Ok(current);
    }
    let upgraded = data_path.join(LIBRARY_DB_OLD);
    if upgraded.is_file() {
        return Ok(upgraded);
    }
    Err(ImportError::BadSource(format!(
        "no library.db or library.db.old at {} or {}",
        current.display(),
        upgraded.display(),
    )))
}

/// Opens one database read-only, and `immutable` unless its `-wal` file holds frames (spec 2.3).
/// No `create_if_missing` and no pragma: sqlx sets none unless asked.
async fn open_read_only(file: &Path) -> Result<SqliteConnection, ImportError> {
    let mut wal = file.as_os_str().to_owned();
    wal.push("-wal");
    let wal_has_frames = std::fs::metadata(&wal).is_ok_and(|meta| meta.len() > 0);
    SqliteConnectOptions::new()
        .filename(file)
        .read_only(true)
        .immutable(!wal_has_frames)
        .connect()
        .await
        .map_err(|e| unreadable(file, &e))
}

/// The 2.9 type of a .NET type name (its last part, with that part); other types are `None`.
fn importable(type_name: &str) -> Option<(MediaType, String)> {
    let last = type_name.rsplit('.').next().unwrap_or(type_name);
    let media_type = match last {
        "Movie" | "Video" => MediaType::Movie,
        "Series" => MediaType::Series,
        "Season" => MediaType::Season,
        "Episode" => MediaType::Episode,
        "Audio" => MediaType::Audio,
        "MusicAlbum" => MediaType::MusicAlbum,
        _ => return None,
    };
    Some((media_type, last.to_string()))
}

/// A database that can't be read: a 400 naming the file, never row data.
fn unreadable(file: &Path, error: &sqlx::Error) -> ImportError {
    ImportError::BadSource(format!("cannot read {}: {error}", file.display()))
}

fn unreadable_path(path: &Path, error: &std::io::Error) -> ImportError {
    ImportError::BadSource(format!("cannot read {}: {error}", path.display()))
}

/// The entries of `dir`, in path order (which is name order for siblings).
fn sorted_children(dir: &Path) -> Result<Vec<PathBuf>, ImportError> {
    let entries = std::fs::read_dir(dir).map_err(|e| unreadable_path(dir, &e))?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| unreadable_path(dir, &e))?;
        paths.push(entry.path());
    }
    paths.sort();
    Ok(paths)
}

fn file_name_text(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The Jellyfin library folders under `P/root/default`, sorted by name (spec 2.8). A missing
/// folder gives none; a folder that can't be read is a 400 naming it.
pub fn read_library_folders(program_data: &Path) -> Result<Vec<LibraryFolder>, ImportError> {
    let root = program_data.join("root").join("default");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut folders = Vec::new();
    for folder in sorted_children(&root)? {
        if !folder.is_dir() {
            continue;
        }
        let mut collection_type = None;
        let mut mblinks = Vec::new();
        for file in sorted_children(&folder)? {
            if !file.is_file() {
                continue;
            }
            let file_name = file_name_text(&file);
            if file_name.ends_with(".mblink") {
                let text =
                    std::fs::read_to_string(&file).map_err(|e| unreadable_path(&file, &e))?;
                let link = text.trim();
                if !link.is_empty() {
                    mblinks.push(link.to_string());
                }
            } else if let Some(kind) = file_name.strip_suffix(".collection") {
                collection_type = collection_type.or_else(|| Some(kind.to_string()));
            }
        }
        folders.push(LibraryFolder {
            name: file_name_text(&folder),
            collection_type,
            mblinks,
        });
    }
    Ok(folders)
}
