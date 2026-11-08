//! Reading a Jellyfin data folder (spec 2.1-2.4, 2.12-2.13). Each database is read with its `-wal`
//! applied and queried from a snapshot (`open_snapshot`): read into memory once, or copied to
//! Rustyfin's data folder when it is too big for memory. The source folder is never written.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use rustyfin_model::image::ImageType;
use rustyfin_model::media::MediaType;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, SqliteConnection};
use tokio::sync::MutexGuard;
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
    /// The metadata of the importable items and their people (spec 5.4, 5.7, 5.8).
    pub metadata: JfMetadata,
}

/// One image of an item or a person (spec 5.8, 5.12).
#[derive(Debug, Clone, PartialEq)]
pub struct JfImage {
    /// Jellyfin's path: a local path, or a `%MetadataPath%` or `%AppDataPath%` form.
    pub path: String,
    pub image_type: ImageType,
    /// Jellyfin's size; `None` when it is not positive.
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub blurhash: Option<String>,
}

/// A credit of an item (spec 5.7), in source row order. Its name and kind come from `person_of`, so
/// the credits of one person share their allocations (B20 round 3, 1).
#[derive(Debug, Clone, PartialEq)]
pub struct JfCredit {
    /// The person's name, trimmed; "" when the source has none.
    pub name: Arc<str>,
    /// The person's `PersonType`, trimmed; None when it is missing, empty or over
    /// `MAX_PERSON_KIND`.
    pub kind: Option<Arc<str>>,
    pub role: Option<String>,
    pub list_order: Option<i64>,
    pub sort_order: Option<i64>,
}

/// The metadata of a Jellyfin item (spec 5.4, 5.8): raw values, which the importer trims and checks.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct JfItemMeta {
    pub forced_sort_name: Option<String>,
    pub original_title: Option<String>,
    pub overview: Option<String>,
    pub tagline: Option<String>,
    pub official_rating: Option<String>,
    pub community_rating: Option<f64>,
    pub critic_rating: Option<f64>,
    pub premiere_date: Option<String>,
    pub end_date: Option<String>,
    pub production_year: Option<i64>,
    /// Jellyfin's pipe-joined lists.
    pub genres: Option<String>,
    pub studios: Option<String>,
    pub tags: Option<String>,
    pub images: Vec<JfImage>,
}

/// The metadata of a source (spec 5.4, 5.7, 5.8).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct JfMetadata {
    /// The importable items (spec 2.9), by Jellyfin id.
    pub items: HashMap<Uuid, JfItemMeta>,
    /// The credits of the importable items, in source row order.
    pub credits: HashMap<Uuid, Vec<JfCredit>>,
    /// The images of Jellyfin's person items by trimmed name; for a repeated name, the first row.
    pub person_images: HashMap<String, Vec<JfImage>>,
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
/// An item row of `BaseItems` (new) or `TypedBaseItems` (old), with its columns aliased to the field
/// names. Only the old layout has `Images`; the new one's images come from `BaseItemImageInfos`.
#[derive(sqlx::FromRow)]
struct MetaDbRow {
    id: Option<Vec<u8>>,
    type_name: String,
    name: Option<String>,
    forced_sort_name: Option<String>,
    original_title: Option<String>,
    overview: Option<String>,
    tagline: Option<String>,
    official_rating: Option<String>,
    community_rating: Option<f64>,
    critic_rating: Option<f64>,
    premiere_date: Option<String>,
    end_date: Option<String>,
    production_year: Option<i64>,
    genres: Option<String>,
    studios: Option<String>,
    tags: Option<String>,
    #[sqlx(default)]
    images: Option<String>,
}
/// `BaseItemImageInfos`: ItemId, Path, ImageType, Width, Height, Blurhash.
type ImageDbRow = (
    Option<Vec<u8>>,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<Vec<u8>>,
);
/// A credit: ItemId, the person's name and kind from `person_of`, Role, ListOrder, SortOrder.
type CreditDbRow = (
    Option<Vec<u8>>,
    Arc<str>,
    Option<Arc<str>>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);
/// An old-layout credit, `People`: ItemId, Name, PersonType, Role, ListOrder, SortOrder.
type OldCreditDbRow = (
    Option<Vec<u8>>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);
/// A person's name and kind, as `person_of` gives them.
type NameAndKind = (Arc<str>, Option<Arc<str>>);
/// A credit link, `PeopleBaseItemMap`: ItemId, PeopleId, Role, ListOrder, SortOrder.
type CreditLinkDbRow = (
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);
/// A person, `Peoples`: Id, Name, PersonType.
type PersonDbRow = (Option<Vec<u8>>, Option<String>, Option<String>);
/// The metadata rows of one layout: items, image rows (new layout only), and credits.
type MetadataRows = (Vec<MetaDbRow>, Vec<ImageDbRow>, Vec<CreditDbRow>);

/// The tables `read_snapshot` reads from `jellyfin.db`, each checked by `check_tables` (B20 round
/// 2, finding 3).
const JELLYFIN_TABLES: &[&str] = &[
    "Users",
    "Permissions",
    "BaseItems",
    "BaseItemProviders",
    "UserData",
    "MediaStreamInfos",
    "BaseItemImageInfos",
    "PeopleBaseItemMap",
    "Peoples",
];
/// The tables `read_old_layout` reads from `library.db`, each checked by `check_tables` (B20 round
/// 2, finding 3).
const LIBRARY_TABLES: &[&str] = &["TypedBaseItems", "UserDatas", "mediastreams", "People"];

/// Refuses a source whose named tables are not plain: a view, a virtual or shadow table, or a table
/// with generated columns can make a query run without end or make values of any size. It also
/// Renders a `quick_check` result line for an error message: each control character becomes
/// `\uXXXX` and the result is cut to 200 bytes on a char boundary, with `…` marking a cut. Keeps a
/// crafted database from writing megabytes of control bytes, or ANSI escapes, into the log.
pub(super) fn clip_check_line(line: &str) -> String {
    use std::fmt::Write as _;
    const MAX: usize = 200;
    let mut out = String::new();
    let mut truncated = false;
    for ch in line.chars() {
        // A control character renders as the six bytes `\uXXXX` (its code point is at most U+009F).
        let piece_len = if ch.is_control() { 6 } else { ch.len_utf8() };
        if out.len() + piece_len > MAX {
            truncated = true;
            break;
        }
        if ch.is_control() {
            let _ = write!(out, "\\u{:04x}", ch as u32);
        } else {
            out.push(ch);
        }
    }
    if truncated {
        out.push('…');
    }
    out
}

/// refuses a damaged table, found with SQLite's `quick_check` before any row is read; the first row
/// is enough and is all it reads. A name the file doesn't have is skipped; its query reports that,
/// as before. `file` is the source's path, never a copy's.
async fn check_tables(
    conn: &mut SqliteConnection,
    file: &Path,
    names: &[&str],
) -> Result<(), ImportError> {
    // A CHECK expression runs for each row that `quick_check` reads, and a crafted one can make
    // values of any size, so CHECK constraints are not run on this connection.
    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(&mut *conn)
        .await
        .map_err(|e| unreadable(file, &e))?;
    for name in names {
        let kind: Option<(String,)> = sqlx::query_as(
            "SELECT type FROM pragma_table_list WHERE schema = 'main' AND name = ?1 \
             COLLATE NOCASE",
        )
        .bind(*name)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| unreadable(file, &e))?;
        let Some((kind,)) = kind else {
            continue;
        };
        if kind != "table" {
            let shown = match kind.as_str() {
                "view" => "view",
                "virtual" => "virtual table",
                "shadow" => "shadow table",
                other => other,
            };
            return Err(ImportError::BadSource(format!(
                "{}: {name} is a {shown}, not a plain table",
                file.display()
            )));
        }
        let (generated,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM pragma_table_xinfo(?1, 'main') WHERE hidden IN (2, 3)",
        )
        .bind(*name)
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| unreadable(file, &e))?;
        if generated > 0 {
            return Err(ImportError::BadSource(format!(
                "{}: {name} has generated columns",
                file.display()
            )));
        }
        // Only the first row is read. SQLite reports the damaged pages there; the rows after it
        // come from a walk over the table that follows the damaged tree.
        let sql = format!("PRAGMA main.quick_check(\"{name}\")");
        let first: Option<(String,)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| unreadable(file, &e))?;
        if let Some((text,)) = first.filter(|(text,)| text.as_str() != "ok") {
            let line = clip_check_line(
                text.lines()
                    .find(|line| !line.starts_with("***"))
                    .unwrap_or(text.as_str()),
            );
            return Err(ImportError::BadSource(format!(
                "{}: {name} is damaged: {line}",
                file.display()
            )));
        }
    }
    Ok(())
}

/// Reads the source's users and importable items (spec 2.2, 2.3). The new layout comes from
/// `jellyfin.db`; the old one from `library.db`, else `library.db.old`. Users always come from
/// `jellyfin.db`. A database too big for memory is copied into `data_dir` while it is read. The
/// tables it reads are checked to be plain tables before any query runs.
pub async fn read_snapshot(
    location: &SourceLocation,
    choice: LayoutChoice,
    data_dir: &Path,
) -> Result<Snapshot, ImportError> {
    let file = location.data_path.join(JELLYFIN_DB);
    let mut conn = open_snapshot(&file, data_dir).await?;
    check_tables(&mut conn, &file, JELLYFIN_TABLES).await?;
    let db = read_jellyfin_db(&mut conn, choice)
        .await
        .map_err(|e| unreadable(&file, &e))?;
    let ((items, user_data, streams), metadata) = match db.layout {
        Layout::New => {
            let metadata = read_new_metadata(&mut conn)
                .await
                .map_err(|e| unreadable(&file, &e))?;
            drop(conn);
            ((db.items, db.user_data, db.streams), metadata)
        }
        Layout::Old => {
            // The old layout's rows come from `library.db`, read next: this snapshot is done with,
            // and a copy of `library.db` would wait for its lock.
            drop(conn);
            read_old_layout(&location.data_path, &db.users, data_dir).await?
        }
    };
    Ok(Snapshot {
        layout: db.layout,
        users: db.users,
        items,
        user_data,
        streams,
        metadata: metadata_of(metadata, db.layout),
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

/// The old layout's items, user-data rows, streams and metadata rows, all from one snapshot of
/// `library.db` (else `library.db.old`). A row reaches every importable item that has its `key`
/// (`UserDataKey`). Only the first row of a key and user counts, as Jellyfin keeps that pair
/// unique. A file that gives more watch data than its items and rows is a bad source.
/// `data_dir` is where a copy goes when the file is too big for memory. Its tables are checked
/// to be plain tables before any query runs.
async fn read_old_layout(
    data_path: &Path,
    users: &[JfUser],
    data_dir: &Path,
) -> Result<(ItemRows, MetadataRows), ImportError> {
    let file = library_file(data_path)?;
    let mut conn = open_snapshot(&file, data_dir).await?;
    check_tables(&mut conn, &file, LIBRARY_TABLES).await?;
    let (item_rows, data_rows, stream_rows) = read_library_db(&mut conn)
        .await
        .map_err(|e| unreadable(&file, &e))?;
    let metadata = read_library_metadata(&mut conn)
        .await
        .map_err(|e| unreadable(&file, &e))?;
    drop(conn);

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
    // The watch data may be at most the importable items plus all the rows.
    let bound = items.len() + data_rows.len();
    // Jellyfin keeps (key, userId) unique, so a row whose pair was seen is skipped.
    let mut seen: HashSet<(String, i64)> = HashSet::new();
    let mut user_data = Vec::new();
    for (key, internal_id, played, plays, position, last, favorite, rating, audio, subtitle) in
        data_rows
    {
        let Some(&user) = internal_ids.get(&internal_id) else {
            continue;
        };
        let on_key = keyed.get(&key).map_or(&[][..], Vec::as_slice);
        if !seen.insert((key, internal_id)) {
            continue;
        }
        if user_data.len() + on_key.len() > bound {
            return Err(ImportError::BadSource(format!(
                "{}: UserDatas gives more watch data than its items and rows",
                file.display()
            )));
        }
        for &item in on_key {
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
    let streams = streams_of(stream_rows, stream_kind_text);
    Ok(((items, user_data, streams), metadata))
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
         rating, AudioStreamIndex, SubtitleStreamIndex FROM UserDatas ORDER BY rowid",
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

/// A Jellyfin person item's type (spec 5.8): its images are the person's, not an item's.
const PERSON_TYPE: &str = "MediaBrowser.Controller.Entities.Person";

/// The images of the old layout's `Images` text (spec 5.8): entries split on `|`, each
/// `Path*Ticks*Type[*Width*Height[*Blurhash]]`, the ticks not read. Fewer than three fields or an
/// empty path is malformed; an ignored or unknown type is skipped. In a blurhash `/` and `\` stand
/// for `*` and `|`.
pub fn parse_old_images(text: &str) -> Vec<JfImage> {
    text.split('|')
        .filter_map(|entry| {
            let fields: Vec<&str> = entry.split('*').collect();
            if fields.len() < 3 || fields[0].is_empty() {
                return None;
            }
            Some(JfImage {
                path: fields[0].to_string(),
                image_type: old_image_type(fields[2])?,
                width: fields
                    .get(3)
                    .and_then(|size| size.parse::<i64>().ok())
                    .and_then(positive_size),
                height: fields
                    .get(4)
                    .and_then(|size| size.parse::<i64>().ok())
                    .and_then(positive_size),
                blurhash: fields
                    .get(5)
                    .map(|hash| hash.replace('/', "*").replace('\\', "|"))
                    .filter(|hash| !hash.is_empty()),
            })
        })
        .collect()
}

/// The longest person name kept, in bytes, after trimming. The longest real values are 334 bytes
/// (names) and 11 bytes (types); the caps bound the work each credit costs (B20 round 3, 1).
const MAX_PERSON_NAME: usize = 1024;
/// The longest person type kept, in bytes, after trimming (see `MAX_PERSON_NAME`).
const MAX_PERSON_KIND: usize = 64;

/// A person's name and kind as a credit holds them (spec 5.7): the name trimmed, "" when NULL, and
/// None when it is over `MAX_PERSON_NAME`; the kind trimmed, and None when it is empty or over
/// `MAX_PERSON_KIND`.
fn person_of(name: Option<String>, kind: Option<String>) -> Option<(Arc<str>, Option<Arc<str>>)> {
    let name = name.as_deref().map(str::trim).unwrap_or_default();
    if name.len() > MAX_PERSON_NAME {
        return None;
    }
    let kind: Option<Arc<str>> = kind
        .as_deref()
        .map(str::trim)
        .filter(|kind| !kind.is_empty() && kind.len() <= MAX_PERSON_KIND)
        .map(Arc::from);
    Some((Arc::from(name), kind))
}

/// The new layout's metadata rows: items, image rows and credits (spec 5.4, 5.7, 5.8). Credits are
/// two plain scans joined here on the person's id, so a repeated id can't multiply them: the first
/// `Peoples` row of an id decides (`person_of` may refuse it, and then the id has no credits), and
/// a link to no known person gives no credit.
async fn read_new_metadata(conn: &mut SqliteConnection) -> Result<MetadataRows, sqlx::Error> {
    let items: Vec<MetaDbRow> = sqlx::query_as(
        "SELECT Id AS id, Type AS type_name, Name AS name, ForcedSortName AS forced_sort_name, \
         OriginalTitle AS original_title, Overview AS overview, Tagline AS tagline, \
         OfficialRating AS official_rating, CommunityRating AS community_rating, \
         CriticRating AS critic_rating, PremiereDate AS premiere_date, EndDate AS end_date, \
         ProductionYear AS production_year, Genres AS genres, Studios AS studios, Tags AS tags \
         FROM BaseItems ORDER BY rowid",
    )
    .fetch_all(&mut *conn)
    .await?;
    let images: Vec<ImageDbRow> = sqlx::query_as(
        "SELECT ItemId, Path, ImageType, Width, Height, Blurhash FROM BaseItemImageInfos \
         ORDER BY rowid",
    )
    .fetch_all(&mut *conn)
    .await?;
    let links: Vec<CreditLinkDbRow> = sqlx::query_as(
        "SELECT ItemId, PeopleId, Role, ListOrder, SortOrder FROM PeopleBaseItemMap \
         ORDER BY rowid",
    )
    .fetch_all(&mut *conn)
    .await?;
    let person_rows: Vec<PersonDbRow> =
        sqlx::query_as("SELECT Id, Name, PersonType FROM Peoples ORDER BY rowid")
            .fetch_all(&mut *conn)
            .await?;
    let mut people: HashMap<Vec<u8>, Option<NameAndKind>> = HashMap::new();
    for (id, name, person_type) in person_rows {
        if let Some(id) = id {
            people
                .entry(id)
                .or_insert_with(|| person_of(name, person_type));
        }
    }
    let credits: Vec<CreditDbRow> = links
        .into_iter()
        .filter_map(|(item, person, role, list_order, sort_order)| {
            let (name, kind) = people.get(&person?)?.as_ref()?;
            Some((
                item,
                Arc::clone(name),
                kind.clone(),
                role,
                list_order,
                sort_order,
            ))
        })
        .collect();
    Ok((items, images, credits))
}

/// The old layout's metadata rows: `TypedBaseItems`, its `Images` text included, and `People`
/// (spec 5.4, 5.7, 5.8). The old layout has no image rows of its own.
async fn read_library_metadata(conn: &mut SqliteConnection) -> Result<MetadataRows, sqlx::Error> {
    let items: Vec<MetaDbRow> = sqlx::query_as(
        "SELECT guid AS id, type AS type_name, Name AS name, ForcedSortName AS forced_sort_name, \
         OriginalTitle AS original_title, Overview AS overview, Tagline AS tagline, \
         OfficialRating AS official_rating, CommunityRating AS community_rating, \
         CriticRating AS critic_rating, PremiereDate AS premiere_date, EndDate AS end_date, \
         ProductionYear AS production_year, Genres AS genres, Studios AS studios, Tags AS tags, \
         Images AS images FROM TypedBaseItems ORDER BY rowid",
    )
    .fetch_all(&mut *conn)
    .await?;
    let rows: Vec<OldCreditDbRow> = sqlx::query_as(
        "SELECT ItemId, Name, PersonType, Role, ListOrder, SortOrder FROM People ORDER BY rowid",
    )
    .fetch_all(&mut *conn)
    .await?;
    // Each row holds its own name, so `person_of` runs once per row, which the file bounds.
    let credits: Vec<CreditDbRow> = rows
        .into_iter()
        .filter_map(|(item, name, kind, role, list_order, sort_order)| {
            let (name, kind) = person_of(name, kind)?;
            Some((item, name, kind, role, list_order, sort_order))
        })
        .collect();
    Ok((items, Vec::new(), credits))
}

/// The image type of a new-layout `ImageType` number (spec 5.8); other numbers are ignored.
fn new_image_type(number: i64) -> Option<ImageType> {
    Some(match number {
        0 => ImageType::Primary,
        1 => ImageType::Art,
        2 => ImageType::Backdrop,
        3 => ImageType::Banner,
        4 => ImageType::Logo,
        5 => ImageType::Thumb,
        6 => ImageType::Disc,
        8 => ImageType::Screenshot,
        _ => return None,
    })
}

/// The image type of an old-layout type name, in exact case (spec 5.8); other names are ignored.
fn old_image_type(name: &str) -> Option<ImageType> {
    Some(match name {
        "Primary" => ImageType::Primary,
        "Art" => ImageType::Art,
        "Backdrop" => ImageType::Backdrop,
        "Banner" => ImageType::Banner,
        "Logo" => ImageType::Logo,
        "Thumb" => ImageType::Thumb,
        "Disc" => ImageType::Disc,
        "Screenshot" => ImageType::Screenshot,
        _ => return None,
    })
}

/// A size as an `i32`, when it is positive (spec 5.8: 0 or less is none).
fn positive_size(value: i64) -> Option<i32> {
    i32::try_from(value).ok().filter(|size| *size > 0)
}

/// The metadata of one layout's rows (spec 5.4, 5.7, 5.8): the importable items with their images,
/// their credits in row order, and the person items' images by trimmed name (the first row of a
/// repeated name).
fn metadata_of(rows: MetadataRows, layout: Layout) -> JfMetadata {
    let (item_rows, image_rows, credit_rows) = rows;
    let mut owned_images: HashMap<Uuid, Vec<JfImage>> = HashMap::new();
    for (owner, path, image_type, width, height, blurhash) in image_rows {
        let Some(owner) = guid_column(owner.as_deref()) else {
            continue;
        };
        let Some(path) = path.filter(|path| !path.is_empty()) else {
            continue;
        };
        let Some(image_type) = image_type.and_then(new_image_type) else {
            continue;
        };
        owned_images.entry(owner).or_default().push(JfImage {
            path,
            image_type,
            width: width.and_then(positive_size),
            height: height.and_then(positive_size),
            blurhash: blurhash
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .filter(|hash| !hash.is_empty()),
        });
    }

    let mut items: HashMap<Uuid, JfItemMeta> = HashMap::new();
    let mut person_images: HashMap<String, Vec<JfImage>> = HashMap::new();
    for row in item_rows {
        let Some(id) = guid_column(row.id.as_deref()) else {
            continue;
        };
        let images = match layout {
            Layout::New => owned_images.remove(&id).unwrap_or_default(),
            Layout::Old => parse_old_images(row.images.as_deref().unwrap_or_default()),
        };
        if row.type_name == PERSON_TYPE {
            let name = row.name.as_deref().map(str::trim).unwrap_or_default();
            if !name.is_empty() {
                person_images.entry(name.to_string()).or_insert(images);
            }
        } else if importable(&row.type_name).is_some() {
            items.insert(
                id,
                JfItemMeta {
                    forced_sort_name: row.forced_sort_name,
                    original_title: row.original_title,
                    overview: row.overview,
                    tagline: row.tagline,
                    official_rating: row.official_rating,
                    community_rating: row.community_rating,
                    critic_rating: row.critic_rating,
                    premiere_date: row.premiere_date,
                    end_date: row.end_date,
                    production_year: row.production_year,
                    genres: row.genres,
                    studios: row.studios,
                    tags: row.tags,
                    images,
                },
            );
        }
    }

    let mut credits: HashMap<Uuid, Vec<JfCredit>> = HashMap::new();
    for (item, name, kind, role, list_order, sort_order) in credit_rows {
        let Some(item) = guid_column(item.as_deref()) else {
            continue;
        };
        if items.contains_key(&item) {
            credits.entry(item).or_default().push(JfCredit {
                name,
                kind,
                role,
                list_order,
                sort_order,
            });
        }
    }
    JfMetadata {
        items,
        credits,
        person_images,
    }
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

/// How long `open_snapshot` waits before it tries the files again (spec Fix 7).
const SNAPSHOT_RETRY: Duration = Duration::from_millis(100);
/// How many tries `open_snapshot` makes before it gives up on files that keep changing.
const SNAPSHOT_TRIES: usize = 5;
/// The piece size in which `same_bytes` and `same_file` compare files, so that neither is ever
/// held twice in memory.
const COMPARE_CHUNK: u64 = 1 << 20;
/// A database and its `-wal` up to this many bytes in all are read into memory; bigger ones are
/// copied into the data folder first (spec Fix 2). SQLite's one buffer for an image refuses more
/// than 2,147,483,391 bytes, so the limit stays below that.
pub(crate) const IN_MEMORY_LIMIT: u64 = 256 << 20;
const _: () = assert!(IN_MEMORY_LIMIT <= 2_147_483_391);
/// The folder under the data folder that holds a copy of a big source while it is read.
const COPY_FOLDER: &str = "jellyfin-import-copy";
/// The copied database and `-wal` in that folder. SQLite finds the `-wal` by its name.
const COPY_DB: &str = "source.db";
const COPY_WAL: &str = "source.db-wal";
/// The copy lock: one copy at a time, so that the disk holds one (spec Fix 2).
static COPY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// The space a copy leaves free on the data folder's disk, at least (spec Fix 3).
const COPY_SPARE: u64 = 1 << 30;

/// Whether a copy of a database of `db_len` bytes and a `-wal` of `wal_len` bytes fits in `free`
/// bytes with `COPY_SPARE` to spare. The sums saturate (spec Fix 3).
pub(crate) fn copy_fits(db_len: u64, wal_len: u64, free: u64) -> bool {
    db_len.saturating_add(wal_len).saturating_add(COPY_SPARE) <= free
}

/// How a copy learns the free space of its folder: `free_space` in the snapshot, and a stand-in in
/// the tests.
pub(crate) type FreeSpace = fn(&Path) -> std::io::Result<u64>;

/// Opens a read-only snapshot of `file` with its `-wal` applied (spec Fix 7, Fix 2). A database
/// and `-wal` up to `IN_MEMORY_LIMIT` bytes in all are read into memory once; bigger ones are
/// copied into `data_dir` for as long as the snapshot is open. SQLite never opens the source, so
/// nothing is created or changed in the source folder, and the snapshot is one state of the files.
pub(crate) async fn open_snapshot(
    file: &Path,
    data_dir: &Path,
) -> Result<SnapshotConn, ImportError> {
    open_snapshot_with(file, data_dir, IN_MEMORY_LIMIT).await
}

/// `open_snapshot` with its in-memory limit given (0 always copies). Each try takes both sizes and
/// picks its path; a try in which the files changed is repeated `SNAPSHOT_RETRY` later, up to
/// `SNAPSHOT_TRIES` tries.
pub(crate) async fn open_snapshot_with(
    file: &Path,
    data_dir: &Path,
    limit: u64,
) -> Result<SnapshotConn, ImportError> {
    open_snapshot_with_free(file, data_dir, limit, free_space).await
}

/// `open_snapshot_with`, with the free space of the copy's folder taken by `free`, which
/// `open_snapshot_with` gives `free_space` (spec Fix 3).
pub(crate) async fn open_snapshot_with_free(
    file: &Path,
    data_dir: &Path,
    limit: u64,
    free: FreeSpace,
) -> Result<SnapshotConn, ImportError> {
    for attempt in 1..=SNAPSHOT_TRIES {
        if let Some(snapshot) = snapshot_try(file, data_dir, limit, free).await? {
            return Ok(snapshot);
        }
        if attempt < SNAPSHOT_TRIES {
            tokio::time::sleep(SNAPSHOT_RETRY).await;
        }
    }
    Err(ImportError::BadSource(format!(
        "{} kept changing while it was read; try again when Jellyfin is idle or stopped",
        file.display()
    )))
}

/// One try of `open_snapshot_with`: None when the files changed while they were read.
async fn snapshot_try(
    file: &Path,
    data_dir: &Path,
    limit: u64,
    free: FreeSpace,
) -> Result<Option<SnapshotConn>, ImportError> {
    let path = file.to_path_buf();
    let (db_len, wal_len) = on_blocking(file, move || source_sizes(&path)).await?;
    if reads_in_memory(db_len, wal_len, limit) {
        let path = file.to_path_buf();
        let Some(image) = on_blocking(file, move || read_once(&path, limit)).await? else {
            return Ok(None);
        };
        let conn = connection_of_image(file, image).await?;
        return Ok(Some(SnapshotConn { conn, _copy: None }));
    }
    // The lock moves into the blocking task that copies, so a request dropped while the copy runs
    // can't let a second copy into the folder. The copy's drop releases it.
    let lock = COPY_LOCK.lock().await;
    let path = file.to_path_buf();
    let folder = data_dir.join(COPY_FOLDER);
    let Some(copy) = on_blocking(file, move || {
        copy_once(&path, &folder, lock, db_len, wal_len, free)
    })
    .await?
    else {
        return Ok(None);
    };
    let conn = SqliteConnectOptions::new()
        .filename(copy.path.join(COPY_DB))
        .read_only(true)
        .connect()
        .await
        .map_err(|e| unreadable(file, &e))?;
    Ok(Some(SnapshotConn {
        conn,
        _copy: Some(copy),
    }))
}

/// Runs `work` on the blocking pool. A task that panics or is cancelled is an error reading `file`.
async fn on_blocking<T: Send + 'static>(
    file: &Path,
    work: impl FnOnce() -> Result<T, ImportError> + Send + 'static,
) -> Result<T, ImportError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| unreadable_path(file, &std::io::Error::other(e)))?
}

/// A read-only in-memory connection to `image`, a database with its `-wal` applied (`read_once`).
async fn connection_of_image(
    file: &Path,
    mut image: Vec<u8>,
) -> Result<SqliteConnection, ImportError> {
    // Header bytes 18 and 19 are the file format versions; 2 means WAL, which a deserialized
    // image refuses. The applied image is a rollback-journal database, so they say 1.
    if image.len() >= 100 {
        image[18] = 1;
        image[19] = 1;
    }
    let mut conn = SqliteConnectOptions::new()
        .connect()
        .await
        .map_err(|e| unreadable(file, &e))?;
    let buf = sqlx::sqlite::SqliteOwnedBuf::try_from(image.as_slice())
        .map_err(|e| unreadable(file, &e))?;
    drop(image);
    conn.deserialize(None, buf, true)
        .await
        .map_err(|e| unreadable(file, &e))?;
    Ok(conn)
}

/// A read-only connection to one snapshot of a source database, for as long as it lives. It
/// derefs to its `SqliteConnection`. On the copy path it also holds the copy, and dropping it
/// releases the connection, removes the copy's folder, then releases the copy lock.
pub(crate) struct SnapshotConn {
    conn: SqliteConnection,
    /// The copy, None on the in-memory path. Only its drop matters.
    _copy: Option<CopyFolder>,
}

impl Deref for SnapshotConn {
    type Target = SqliteConnection;

    fn deref(&self) -> &SqliteConnection {
        &self.conn
    }
}

impl DerefMut for SnapshotConn {
    fn deref_mut(&mut self) -> &mut SqliteConnection {
        &mut self.conn
    }
}

/// A copy's folder and the lock that lets only one copy exist. Dropping it removes the folder,
/// then releases the lock, which is the last field and so drops last.
struct CopyFolder {
    path: PathBuf,
    _lock: MutexGuard<'static, ()>,
}

impl Drop for CopyFolder {
    fn drop(&mut self) {
        // Best effort: a folder that stays is removed when the next copy starts.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Whether a database of `db_len` bytes with a `-wal` of `wal_len` bytes is read into memory: the
/// image is at most `limit` bytes (spec Fix 2). The applied image is at most the two files together,
/// since the applier refuses a commit that names more pages, so it fits SQLite's buffer whenever
/// `limit` does.
pub(crate) fn reads_in_memory(db_len: u64, wal_len: u64, limit: u64) -> bool {
    db_len.saturating_add(wal_len) <= limit
}

/// One read of `file` and its `-wal`, in this order: the `-wal` head, the database, the `-wal`,
/// the database again (compared byte for byte), the `-wal` head again. None when the files changed
/// in between: the two heads differ, or the `-wal`'s head is not the one read first. None too when
/// the database and `-wal` are more than `limit` bytes in all, or either is more than `limit` alone,
/// so no more than `limit + 1` bytes of either are ever held.
fn read_once(file: &Path, limit: u64) -> Result<Option<Vec<u8>>, ImportError> {
    let wal_file = wal_path(file);
    let head1 = wal_head(&wal_file)?;
    let Some(db) = read_up_to(file, open_database(file)?, limit)? else {
        return Ok(None);
    };
    let wal = match open_regular(&wal_file)? {
        Some(handle) => read_up_to(&wal_file, handle, limit)?,
        None => Some(Vec::new()),
    };
    let Some(wal) = wal else {
        return Ok(None);
    };
    if (db.len() as u64).saturating_add(wal.len() as u64) > limit {
        return Ok(None);
    }
    let same = same_bytes(file, &db)?;
    let head2 = wal_head(&wal_file)?;
    let wal_matches = head1.as_ref().map(|head| &head[..]) == wal.get(..32);
    if same && head1 == head2 && wal_matches {
        apply_wal(file, db, &wal).map(Some)
    } else {
        Ok(None)
    }
}

/// Opens `path` if it is a regular file (spec Fix 1). A missing file gives None, and so does one
/// that goes missing between the check and the open. Any other file (a FIFO, a device, a folder)
/// is a BadSource that names it, found before anything is read from it, so that nothing blocks on
/// it or reads from it. `std::fs::metadata` follows links, so a link to a regular file is fine. The
/// open handle is checked as well.
fn open_regular(path: &Path) -> Result<Option<File>, ImportError> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(not_regular(path)),
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unreadable_path(path, &e)),
    }
    let handle = match File::open(path) {
        Ok(handle) => handle,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unreadable_path(path, &e)),
    };
    match handle.metadata() {
        Ok(meta) if meta.is_file() => Ok(Some(handle)),
        Ok(_) => Err(not_regular(path)),
        Err(e) => Err(unreadable_path(path, &e)),
    }
}

/// The error for a source that is not a regular file.
fn not_regular(path: &Path) -> ImportError {
    ImportError::BadSource(format!("{} is not a regular file", path.display()))
}

/// The database `file`, opened through `open_regular`. A database that is not there is the
/// NotFound error it has always been, not a retry.
fn open_database(file: &Path) -> Result<File, ImportError> {
    open_regular(file)?
        .ok_or_else(|| unreadable_path(file, &std::io::Error::from(ErrorKind::NotFound)))
}

/// The bytes of `handle`, an open `file`, when there are no more than `limit` of them; None when
/// there are more. The read stops at `limit + 1` bytes, so a file that grew is never read whole.
fn read_up_to(file: &Path, handle: File, limit: u64) -> Result<Option<Vec<u8>>, ImportError> {
    let mut bytes = Vec::new();
    handle
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| unreadable_path(file, &e))?;
    Ok((bytes.len() as u64 <= limit).then_some(bytes))
}

/// The sizes of the database `file` and of its `-wal` (0 when there is none), each taken from an
/// open handle through `open_regular`.
fn source_sizes(file: &Path) -> Result<(u64, u64), ImportError> {
    let db_len = open_database(file)?
        .metadata()
        .map_err(|e| unreadable_path(file, &e))?
        .len();
    let wal_file = wal_path(file);
    let wal_len = match open_regular(&wal_file)? {
        Some(wal) => wal
            .metadata()
            .map_err(|e| unreadable_path(&wal_file, &e))?
            .len(),
        None => 0,
    };
    Ok((db_len, wal_len))
}

/// One copy of `file` and its `-wal` into `folder`, made while `lock` is held (spec Fix 2). A folder
/// that a stopped copy left behind is removed first, then the folder is made private. Once it is
/// made, `free` must find room for `db_len` and `wal_len` bytes with 1 GiB to spare, else it is a
/// BadSource (spec Fix 3). Each file is copied at most its measured length, and more bytes than that
/// is None, since the files changed. The result holds the folder, which removes itself when
/// dropped. None when the copy is not one state of the files, and the folder is removed then too.
fn copy_once(
    file: &Path,
    folder: &Path,
    lock: MutexGuard<'static, ()>,
    db_len: u64,
    wal_len: u64,
    free: FreeSpace,
) -> Result<Option<CopyFolder>, ImportError> {
    match std::fs::remove_dir_all(folder) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(copy_error(file, folder, &e)),
    }
    create_private_dir(folder).map_err(|e| copy_error(file, folder, &e))?;
    // From here on, each return drops `copy`: the folder goes first, then the lock.
    let copy = CopyFolder {
        path: folder.to_path_buf(),
        _lock: lock,
    };
    let space = free(folder).map_err(|e| copy_error(file, folder, &e))?;
    if !copy_fits(db_len, wal_len, space) {
        let needs = db_len.saturating_add(wal_len).div_ceil(1 << 20);
        let free_mib = space >> 20;
        return Err(ImportError::BadSource(format!(
            "not enough free space to copy {} to {}: it needs {needs} MiB and 1 GiB to spare, \
             and {free_mib} MiB are free",
            file.display(),
            folder.display()
        )));
    }
    let wal_file = wal_path(file);
    let head1 = wal_head(&wal_file)?;
    if !copy_bytes(file, folder, db_len, wal_len)? {
        return Ok(None);
    }
    let same = same_file(file, folder)?;
    let head2 = wal_head(&wal_file)?;
    let copied_head = wal_head(&folder.join(COPY_WAL))?;
    if same && head1 == head2 && copied_head == head1 {
        Ok(Some(copy))
    } else {
        Ok(None)
    }
}

/// Copies the database at `file` to `folder/source.db`, and its `-wal`, if it has one, to
/// `folder/source.db-wal`, each at most its measured length: `db_len` and `wal_len`. False as soon
/// as one has more bytes than that, since the files changed (spec Fix 3). A `-wal` that is not a
/// regular file is a BadSource, as in `open_regular`.
fn copy_bytes(file: &Path, folder: &Path, db_len: u64, wal_len: u64) -> Result<bool, ImportError> {
    let db = open_database(file)?;
    if !copy_into(file, folder, db, &folder.join(COPY_DB), db_len)? {
        return Ok(false);
    }
    match open_regular(&wal_path(file))? {
        Some(wal) => copy_into(file, folder, wal, &folder.join(COPY_WAL), wal_len),
        None => Ok(true),
    }
}

/// Copies `from` into a new private file at `to`, at most `len` bytes (`copy_at_most`). False when
/// `from` had more, so the copy is not one state of the files (spec Fix 3).
fn copy_into(
    file: &Path,
    folder: &Path,
    mut from: File,
    to: &Path,
    len: u64,
) -> Result<bool, ImportError> {
    let mut into = create_private_file(to).map_err(|e| copy_error(file, folder, &e))?;
    copy_at_most(&mut from, &mut into, len).map_err(|e| copy_error(file, folder, &e))
}

/// Copies at most `len` bytes from `from` into `into`, then reads one more byte. False when there
/// was one: `from` had more than `len` bytes, so the files changed. Fewer bytes are copied as they
/// are, as `read_up_to` does on the in-memory path (spec Fix 3).
pub(crate) fn copy_at_most(
    from: &mut impl Read,
    into: &mut impl std::io::Write,
    len: u64,
) -> std::io::Result<bool> {
    std::io::copy(&mut (&mut *from).take(len), into)?;
    Ok(std::io::copy(&mut from.take(1), &mut std::io::sink())? == 0)
}

/// Whether the database at `file` and its copy in `folder` hold the same bytes, compared in
/// `COMPARE_CHUNK` pieces so that neither is held in memory whole.
fn same_file(file: &Path, folder: &Path) -> Result<bool, ImportError> {
    let mut source = open_database(file)?;
    let mut copy = File::open(folder.join(COPY_DB)).map_err(|e| copy_error(file, folder, &e))?;
    let (mut left, mut right) = (Vec::new(), Vec::new());
    loop {
        left.clear();
        right.clear();
        (&mut source)
            .take(COMPARE_CHUNK)
            .read_to_end(&mut left)
            .map_err(|e| unreadable_path(file, &e))?;
        (&mut copy)
            .take(COMPARE_CHUNK)
            .read_to_end(&mut right)
            .map_err(|e| copy_error(file, folder, &e))?;
        if left != right {
            return Ok(false);
        }
        if left.is_empty() {
            return Ok(true);
        }
    }
}

/// The error for a copy of `file` into `folder` that failed.
fn copy_error(file: &Path, folder: &Path, error: &std::io::Error) -> ImportError {
    ImportError::BadSource(format!(
        "cannot copy {} to {}: {error}",
        file.display(),
        folder.display()
    ))
}

/// The free space in bytes that a user without privileges can use on the disk of `dir`: `f_bavail`
/// blocks of `f_frsize` bytes, saturating (spec Fix 3).
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
pub(crate) fn free_space(dir: &Path) -> std::io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(ErrorKind::InvalidInput, e))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a NUL-terminated string that lives for the call, and `stat` is writable.
    let result = unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `statvfs` returned 0, so it has filled in `stat`.
    let stat = unsafe { stat.assume_init() };
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

/// Without `statvfs` every copy is taken to fit.
#[cfg(not(unix))]
pub(crate) fn free_space(_dir: &Path) -> std::io::Result<u64> {
    Ok(u64::MAX)
}

/// Creates `path` as a new folder that only this user can use (mode 0700).
#[cfg(unix)]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir(path)
}

/// Creates `path` as a new file for writing, which only this user can read (mode 0600). It must not
/// exist already, so a planted link is never followed.
#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// The `-wal` file of a database: its path with `-wal` appended.
fn wal_path(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

/// The first 32 bytes of the `-wal` at `wal`; None when the file is missing or shorter. A `-wal`
/// that is not a regular file is a BadSource (`open_regular`).
fn wal_head(wal: &Path) -> Result<Option<[u8; 32]>, ImportError> {
    let Some(mut handle) = open_regular(wal)? else {
        return Ok(None);
    };
    let mut head = [0u8; 32];
    match handle.read_exact(&mut head) {
        Ok(()) => Ok(Some(head)),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(unreadable_path(wal, &e)),
    }
}

/// Whether `file` holds exactly `expected`, compared in `COMPARE_CHUNK` pieces so that the file is
/// never held twice in memory.
fn same_bytes(file: &Path, expected: &[u8]) -> Result<bool, ImportError> {
    let mut handle = open_database(file)?;
    let mut chunk = Vec::new();
    let mut offset = 0;
    loop {
        chunk.clear();
        handle
            .by_ref()
            .take(COMPARE_CHUNK)
            .read_to_end(&mut chunk)
            .map_err(|e| unreadable_path(file, &e))?;
        if chunk.is_empty() {
            return Ok(offset == expected.len());
        }
        let part = expected
            .get(offset..)
            .and_then(|rest| rest.get(..chunk.len()));
        if part != Some(chunk.as_slice()) {
            return Ok(false);
        }
        offset += chunk.len();
    }
}

/// `file`'s image with `wal` applied as SQLite's recovery applies it (file format section 4): only
/// the frames up to the last valid commit frame count. `db` is returned as it is when none does.
/// Errors name the file and say nothing about its data.
fn apply_wal(file: &Path, mut db: Vec<u8>, wal: &[u8]) -> Result<Vec<u8>, ImportError> {
    // The header is 32 bytes. A short one, a magic or page size that isn't SQLite's, or a checksum
    // that fails means no frames at all.
    let (Some(magic), Some(version), Some(page), Some(sum0), Some(sum1)) = (
        be32(wal, 0),
        be32(wal, 4),
        be32(wal, 8),
        be32(wal, 24),
        be32(wal, 28),
    ) else {
        return Ok(db);
    };
    let big_endian = match magic {
        0x377f_0683 => true,
        0x377f_0682 => false,
        _ => return Ok(db),
    };
    let page = page as usize;
    if !page.is_power_of_two() || !(512..=65_536).contains(&page) {
        return Ok(db);
    }
    // The header is 32 bytes long, so its first 24 are there to checksum.
    if checksum(&wal[..24], big_endian, (0, 0)) != (sum0, sum1) {
        return Ok(db);
    }
    if version != 3_007_000 {
        return Err(bad_wal(
            file,
            "its -wal has a format version this import does not read",
        ));
    }

    // The frames from offset 32 while a whole one fits. `frames` holds the valid ones as (page
    // number, offset of the page); `commit` is the size of the last commit frame and how many
    // frames it covers.
    let frame_len = 24 + page;
    let salts = &wal[16..24];
    let mut running = (sum0, sum1);
    let mut frames: Vec<(u32, usize)> = Vec::new();
    let mut commit: Option<(u32, usize)> = None;
    let mut at = 32;
    while let Some(frame) = wal.get(at..).and_then(|rest| rest.get(..frame_len)) {
        let (Some(number), Some(size)) = (be32(frame, 0), be32(frame, 4)) else {
            break;
        };
        if number == 0 || frame[8..16] != *salts {
            break;
        }
        let sum = checksum(&frame[..8], big_endian, running);
        let sum = checksum(&frame[24..], big_endian, sum);
        if be32(frame, 16) != Some(sum.0) || be32(frame, 20) != Some(sum.1) {
            break;
        }
        running = sum;
        frames.push((number, at + 24));
        if size != 0 {
            commit = Some((size, frames.len()));
        }
        at += frame_len;
    }
    let Some((commit_size, applied)) = commit else {
        return Ok(db);
    };
    frames.truncate(applied);

    // A non-empty database file must be a whole database of the -wal's page size; an empty one is
    // built from the frames alone. The commit may not name more pages than the file and frames give.
    let fits = db.is_empty() || (db.len() >= 100 && db_page_size(&db) == Some(page));
    if !fits {
        return Err(bad_wal(file, "its database file does not match its -wal"));
    }
    let pages = commit_size as usize;
    if pages > db.len().div_ceil(page) + applied {
        return Err(bad_wal(
            file,
            "its -wal commits more pages than its database holds",
        ));
    }
    let size = pages
        .checked_mul(page)
        .ok_or_else(|| bad_wal(file, "its -wal commits more pages than fit in memory"))?;
    db.resize(size, 0);
    for (number, start) in frames {
        let number = number as usize;
        if number > pages {
            continue;
        }
        let dest = (number - 1) * page;
        if let (Some(out), Some(data)) = (
            db.get_mut(dest..).and_then(|rest| rest.get_mut(..page)),
            wal.get(start..).and_then(|rest| rest.get(..page)),
        ) {
            out.copy_from_slice(data);
        }
    }
    Ok(db)
}

/// A `-wal` that can't be applied to its database: the message names the file and the problem.
fn bad_wal(file: &Path, problem: &str) -> ImportError {
    ImportError::BadSource(format!("{}: {problem}", file.display()))
}

/// The big-endian u32 at byte `at` of `bytes`, or None when four bytes don't lie there.
fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    let word: [u8; 4] = bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_be_bytes(word))
}

/// The page size in a database file's header (bytes 16 and 17, where 1 means 65536).
fn db_page_size(db: &[u8]) -> Option<usize> {
    let size = u16::from_be_bytes(db.get(16..18)?.try_into().ok()?);
    Some(if size == 1 { 65_536 } else { usize::from(size) })
}

/// SQLite's WAL checksum (file format section 4.2) over the whole 8-byte pairs of `data`, continuing
/// from `sum`. The words are little-endian for the magic 0x377f0682 and big-endian for 0x377f0683.
fn checksum(data: &[u8], big_endian: bool, mut sum: (u32, u32)) -> (u32, u32) {
    for pair in data.as_chunks::<8>().0 {
        let word = |at: usize| {
            let bytes = [pair[at], pair[at + 1], pair[at + 2], pair[at + 3]];
            if big_endian {
                u32::from_be_bytes(bytes)
            } else {
                u32::from_le_bytes(bytes)
            }
        };
        sum.0 = sum.0.wrapping_add(word(0)).wrapping_add(sum.1);
        sum.1 = sum.1.wrapping_add(word(4)).wrapping_add(sum.0);
    }
    sum
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

/// The most bytes a `.mblink` may hold. A `.mblink` holds one path, and the cap keeps a link to a
/// big file, such as a film, from being read whole.
const MBLINK_LIMIT: u64 = 64 << 10;

/// The most `.mblink` text one source may hold, across all its library folders. A link holds one
/// path (at most PATH_MAX); the cap keeps a folder of many links from holding gigabytes of text.
const MBLINK_TOTAL: u64 = 1 << 20;

/// The Jellyfin library folders under `P/root/default`, sorted by name (spec 2.8). A missing
/// folder gives none; a folder that can't be read is a 400 naming it. A `.mblink` over 64 KiB is
/// a 400 naming it, as is a source whose `.mblink` text tops 1 MiB in total (naming the folder
/// that crossed it).
pub fn read_library_folders(program_data: &Path) -> Result<Vec<LibraryFolder>, ImportError> {
    let root = program_data.join("root").join("default");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut folders = Vec::new();
    let mut mblink_total: u64 = 0;
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
                let Some(handle) = open_regular(&file)? else {
                    continue;
                };
                let Some(bytes) = read_up_to(&file, handle, MBLINK_LIMIT)? else {
                    return Err(ImportError::BadSource(format!(
                        "{} is longer than 64 KiB; a .mblink holds one path",
                        file.display()
                    )));
                };
                let text = String::from_utf8(bytes).map_err(|e| {
                    unreadable_path(&file, &std::io::Error::new(ErrorKind::InvalidData, e))
                })?;
                let link = text.trim();
                if !link.is_empty() {
                    mblink_total = mblink_total.saturating_add(link.len() as u64);
                    if mblink_total > MBLINK_TOTAL {
                        return Err(ImportError::BadSource(format!(
                            "{}: its .mblink files hold more than 1 MiB of paths",
                            folder.display()
                        )));
                    }
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
