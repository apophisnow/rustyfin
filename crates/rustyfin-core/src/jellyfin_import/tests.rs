//! B20: unit tests for the Jellyfin importer's pure parts (written with the spec, before the code).

use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};
use rustyfin_db::repo_jellyfin_import::{SourceStreams, StreamRow, UserDataRow};
use rustyfin_model::image::ImageType;
use uuid::Uuid;

use super::images::{resolve, signature};
use super::job::{ImportJob, ImportStage, ImportStatus, JobState};
use super::merge::{MappedTracks, convert_rating, harmonize, merge};
use super::paths::{PathMapping, suggest_metadata, translate_path};
use super::plan::{UserMatch, match_user_name};
use super::source::{
    JfImage, JfStream, JfUserData, ProviderIds, StreamKind, guid_from_blob, guid_from_text, locate,
    open_snapshot, parse_jellyfin_date, parse_old_images, parse_provider_ids,
};
use super::tracks::{choose_source, map_track, normalize_codec};
use super::{
    ImportError, ImportReport, ItemReport, Layout, TrackReport, UserAction, UserRow,
    WatchDataReport,
};

/// A temp folder removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir =
            std::env::temp_dir().join(format!("rustyfin_b20_unit_{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// Creates an empty `jellyfin.db` in `rel` (and its folders) and returns that folder.
    fn db_in(&self, rel: &str) -> PathBuf {
        let dir = self.0.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("jellyfin.db"), b"").unwrap();
        dir
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn pair(from: &str, to: &str) -> PathMapping {
    PathMapping {
        from: from.to_string(),
        to: to.to_string(),
    }
}

// 2.8: component-wise prefixes, longest From wins, trailing slashes ignored.
#[test]
fn b20_translate_path_by_longest_component_prefix() {
    let pairs = vec![
        pair("/data/movies", "/srv/films"),
        pair("/data/tv/", "/srv/tv"),
        pair("/data/movies/kids/", "/srv/kids/"),
        pair("%MetadataPath%", "/cfg/metadata"),
    ];
    let t = |p: &str| translate_path(p, &pairs);
    assert_eq!(
        t("/data/movies/A (2001)/A.mkv").as_deref(),
        Some("/srv/films/A (2001)/A.mkv")
    );
    assert_eq!(
        t("/data/movies/kids/B/B.mkv").as_deref(),
        Some("/srv/kids/B/B.mkv")
    );
    assert_eq!(t("/data/movies").as_deref(), Some("/srv/films"));
    assert_eq!(
        t("/data/tv/Show/Season 01").as_deref(),
        Some("/srv/tv/Show/Season 01")
    );
    assert_eq!(
        t("%MetadataPath%/library/ab/abcd/poster.jpg").as_deref(),
        Some("/cfg/metadata/library/ab/abcd/poster.jpg")
    );
    assert_eq!(t("/data/movies2/C.mkv"), None, "not a component prefix");
    assert_eq!(
        t("/Data/Movies/A.mkv"),
        None,
        "paths compare case-sensitively"
    );
    assert_eq!(t("/elsewhere/x.mkv"), None);
    assert_eq!(translate_path("/data/movies/A.mkv", &[]), None);
}

// 2.13: Jellyfin's date formats, all UTC; anything else is no date.
#[test]
fn b20_parse_jellyfin_dates() {
    let base = Utc.with_ymd_and_hms(2025, 1, 2, 3, 4, 5).unwrap();
    let frac = base + chrono::Duration::nanoseconds(123_456_700);
    assert_eq!(
        parse_jellyfin_date("2025-01-02 03:04:05.1234567"),
        Some(frac)
    );
    assert_eq!(
        parse_jellyfin_date("2025-01-02 03:04:05.1234567Z"),
        Some(frac)
    );
    assert_eq!(parse_jellyfin_date("2025-01-02 03:04:05"), Some(base));
    assert_eq!(parse_jellyfin_date("2025-01-02 03:04:05Z"), Some(base));
    assert_eq!(
        parse_jellyfin_date("2025-01-02 03:04:05.5"),
        Some(base + chrono::Duration::milliseconds(500))
    );
    assert_eq!(
        parse_jellyfin_date("2025-01-02T03:04:05.1234567Z"),
        Some(frac)
    );
    assert_eq!(parse_jellyfin_date("2025-01-02T05:04:05+02:00"), Some(base));
    assert_eq!(parse_jellyfin_date(""), None);
    assert_eq!(parse_jellyfin_date("never"), None);
    assert_eq!(parse_jellyfin_date("2025-02-30 00:00:00"), None);
}

// 2.3a: GUIDs as TEXT in any case, or as 16-byte BLOBs in .NET byte order.
#[test]
fn b20_guids_from_text_and_blobs() {
    let id = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
    assert_eq!(
        guid_from_text("00112233-4455-6677-8899-AABBCCDDEEFF"),
        Some(id)
    );
    assert_eq!(
        guid_from_text("00112233-4455-6677-8899-aabbccddeeff"),
        Some(id)
    );
    assert_eq!(guid_from_text("not a guid"), None);
    assert_eq!(guid_from_text(""), None);
    let dotnet = [
        0x33, 0x22, 0x11, 0x00, 0x55, 0x44, 0x77, 0x66, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    assert_eq!(guid_from_blob(&dotnet), Some(id));
    assert_eq!(guid_from_blob(&dotnet[..15]), None);
    assert_eq!(guid_from_blob(&[]), None);
}

// 2.3a: provider ids parse leniently; bad values are ignored, not errors.
#[test]
fn b20_provider_ids_parse_leniently() {
    assert_eq!(
        parse_provider_ids("Tmdb=1|Imdb=tt0079540|Tvdb=3"),
        ProviderIds {
            tmdb: Some(1),
            tvdb: Some(3),
            imdb: Some("tt0079540".to_string()),
        }
    );
    assert_eq!(
        parse_provider_ids("Tmdb=abc|Imdb=nm0000123|Tvdb=-5|Zap2It=EP1|garbage|=7"),
        ProviderIds::default()
    );
    assert_eq!(parse_provider_ids("Imdb=tt|Tmdb=12a").imdb, None);
    assert_eq!(parse_provider_ids("tmdb=7").tmdb, Some(7));
    assert_eq!(parse_provider_ids(""), ProviderIds::default());

    let mut ids = ProviderIds::default();
    ids.set("Tvdb", "333");
    ids.set("Tmdb", "x1");
    ids.set("Imdb", "tt42");
    ids.set("MusicBrainzAlbum", "123");
    assert_eq!(
        ids,
        ProviderIds {
            tmdb: None,
            tvdb: Some(333),
            imdb: Some("tt42".to_string()),
        }
    );
}

// 2.1: jellyfin.db at <s>, <s>/data or <s>/data/data; D holds it, P is D's parent.
#[test]
fn b20_locate_finds_the_database_in_three_places() {
    let tmp = TempDir::new();
    let a = tmp.db_in("a");
    let found = locate(&a).unwrap();
    assert_eq!(found.data_path, a);
    assert_eq!(found.program_data_path, tmp.0);

    let b = tmp.db_in("b/data");
    let found = locate(&tmp.0.join("b")).unwrap();
    assert_eq!(found.data_path, b);
    assert_eq!(found.program_data_path, tmp.0.join("b"));

    let c = tmp.db_in("c/data/data");
    let found = locate(&tmp.0.join("c")).unwrap();
    assert_eq!(found.data_path, c);
    assert_eq!(found.program_data_path, tmp.0.join("c/data"));

    // The first hit wins.
    tmp.db_in("b");
    assert_eq!(locate(&tmp.0.join("b")).unwrap().data_path, tmp.0.join("b"));
}

#[test]
fn b20_locate_rejects_bad_sources() {
    let tmp = TempDir::new();
    let empty = tmp.0.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    match locate(&empty) {
        Err(ImportError::BadSource(msg)) => {
            for candidate in ["jellyfin.db", "data/jellyfin.db", "data/data/jellyfin.db"] {
                let path = empty.join(candidate);
                assert!(
                    msg.contains(path.to_str().unwrap()),
                    "{msg} names {candidate}"
                );
            }
        }
        other => panic!("expected BadSource, got {other:?}"),
    }
    for bad in ["", "relative/config"] {
        assert!(
            matches!(locate(Path::new(bad)), Err(ImportError::BadSource(_))),
            "{bad:?}"
        );
    }
    assert!(matches!(
        locate(&tmp.0.join("missing")),
        Err(ImportError::BadSource(_))
    ));
}

// 2.5: case-insensitive names; an exact-case match wins; otherwise two hits are ambiguous.
#[test]
fn b20_match_user_names() {
    let (alice, bob_title, bob_lower, unal) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let users = vec![
        (alice, "alice".to_string()),
        (bob_title, "Bob".to_string()),
        (bob_lower, "bob".to_string()),
        (unal, "Ünal".to_string()),
    ];
    assert_eq!(match_user_name("Alice", &users), UserMatch::Matched(alice));
    assert_eq!(
        match_user_name("bob", &users),
        UserMatch::Matched(bob_lower)
    );
    assert_eq!(
        match_user_name("Bob", &users),
        UserMatch::Matched(bob_title)
    );
    assert_eq!(match_user_name("BOB", &users), UserMatch::Ambiguous);
    assert_eq!(match_user_name("ünal", &users), UserMatch::Matched(unal));
    assert_eq!(match_user_name("carol", &users), UserMatch::NoMatch);
}

// ── 2b: the import job (spec 2.19, 2.21, 2.25) ──

/// A small report whose one created user holds `code`.
fn b20_report_with_code(code: &str) -> ImportReport {
    ImportReport {
        layout: Layout::New,
        data_path: "/config/data".to_string(),
        path_mappings: Vec::new(),
        suggested_path_mappings: Vec::new(),
        unpaired_library_paths: Vec::new(),
        users: vec![UserRow {
            jellyfin_id: Uuid::from_u128(7),
            name: "bob".to_string(),
            was_administrator: false,
            action: UserAction::Create,
            reason: None,
            user_id: Some(Uuid::from_u128(8)),
            code: Some(code.to_string()),
        }],
        items: ItemReport {
            total: 0,
            matched_by_path: 0,
            matched_by_id: 0,
            unmatched: 0,
            unmatched_samples: Vec::new(),
        },
        watch_data: WatchDataReport::default(),
        tracks: TrackReport::default(),
        metadata: Default::default(),
    }
}

#[test]
fn b20_job_is_idle_at_first() {
    let status = ImportJob::default().status();
    assert_eq!((status.state, status.stage), (JobState::Idle, None));
    assert_eq!((status.done, status.total), (0, 0));
    assert!(status.result.is_none() && status.error.is_none());
}

#[test]
fn b20_job_runs_one_at_a_time() {
    let job = ImportJob::default();
    let clone = job.clone();
    let handle = job.try_start().expect("an idle job starts");
    assert!(clone.try_start().is_none(), "a second run while one runs");
    let status = clone.status();
    assert_eq!(
        (status.state, status.stage),
        (JobState::Running, Some(ImportStage::Reading))
    );
    handle.stage(ImportStage::Users, 3);
    handle.progress(2);
    let status = job.status();
    assert_eq!(status.stage, Some(ImportStage::Users));
    assert_eq!((status.done, status.total), (2, 3));
    handle.stage(ImportStage::Matching, 10);
    let status = job.status();
    assert_eq!(
        (status.done, status.total),
        (0, 10),
        "a new stage starts at 0"
    );
    handle.complete(b20_report_with_code("ABCD-EFGH-JKMN"));
    let status = job.status();
    assert_eq!((status.state, status.stage), (JobState::Completed, None));
    assert!(status.error.is_none());

    let again = job.try_start().expect("a finished job starts again");
    let status = job.status();
    assert_eq!(status.state, JobState::Running);
    assert!(status.result.is_none(), "a new run drops the old report");
    again.fail("stopped".to_string());
}

#[test]
fn b20_job_shows_codes_once() {
    let job = ImportJob::default();
    let handle = job.try_start().unwrap();
    handle.complete(b20_report_with_code("ABCD-EFGH-JKMN"));
    let code = |status: &ImportStatus| status.result.as_ref().unwrap().users[0].code.clone();
    let first = job.status();
    assert_eq!(code(&first), Some("ABCD-EFGH-JKMN".to_string()));
    let second = job.status();
    assert_eq!(second.state, JobState::Completed);
    assert_eq!(code(&second), None, "the code is shown once");
    let user_id = second.result.unwrap().users[0].user_id;
    assert_eq!(
        user_id,
        Some(Uuid::from_u128(8)),
        "the rest of the report stays"
    );
}

#[test]
fn b20_job_reports_failures() {
    let job = ImportJob::default();
    job.try_start().unwrap().fail("cannot read x".to_string());
    let status = job.status();
    assert_eq!((status.state, status.stage), (JobState::Failed, None));
    assert_eq!(status.error.as_deref(), Some("cannot read x"));
    assert!(status.result.is_none());

    // A handle dropped without an ending fails the run; the job starts again.
    drop(job.try_start().unwrap());
    let status = job.status();
    assert_eq!(status.state, JobState::Failed);
    assert_eq!(
        status.error.as_deref(),
        Some("the import stopped before it finished")
    );
    assert!(job.try_start().is_some());
}

// ── 2c: watch data and track choices (spec 2.12-2.16, 2.28-2.30) ──

/// Noon of January `day`, 2025.
fn at(day: u32) -> Option<DateTime<Utc>> {
    Some(Utc.with_ymd_and_hms(2025, 1, day, 12, 0, 0).unwrap())
}

/// A Jellyfin row of `user` for `item` with nothing set.
fn jf_row(user: u128, item: u128) -> JfUserData {
    JfUserData {
        item: Uuid::from_u128(item),
        user: Uuid::from_u128(user),
        played: false,
        play_count: 0,
        position_ticks: 0,
        last_played: None,
        is_favorite: false,
        rating: None,
        likes: None,
        audio: None,
        subtitle: None,
    }
}

/// User 1's row of item 10: played twice, last on January 5, stopped at 600 ticks.
fn jf_watched() -> JfUserData {
    JfUserData {
        played: true,
        play_count: 2,
        position_ticks: 600,
        last_played: at(5),
        is_favorite: true,
        rating: Some(7.6),
        likes: Some(true),
        audio: Some(1),
        subtitle: Some(3),
        ..jf_row(1, 10)
    }
}

/// The Rustyfin indexes of `jf_watched`'s track choices.
const MAPPED: MappedTracks = MappedTracks {
    audio: Some(2),
    audio_key: None,
    subtitle: Some(4),
    subtitle_key: None,
};

#[test]
fn b20_normalize_codecs() {
    for (codec, normal) in [
        ("PGSSUB", "hdmv_pgs_subtitle"),
        ("hdmv_pgs_subtitle", "hdmv_pgs_subtitle"),
        ("dvdsub", "dvd_subtitle"),
        ("DVBSUB", "dvb_subtitle"),
        ("srt", "subrip"),
        ("SubRip", "subrip"),
        ("AAC", "aac"),
        ("eac3", "eac3"),
    ] {
        assert_eq!(normalize_codec(codec), normal, "{codec}");
    }
}

#[test]
fn b20_convert_ratings() {
    for (rating, converted) in [
        (7.6, Some(8)),
        (7.5, Some(8)),
        (7.4, Some(7)),
        (9.5, Some(10)),
        (0.0, Some(0)),
        (10.0, Some(10)),
        (-1.0, Some(0)),
        (11.2, Some(10)),
        (f64::INFINITY, Some(10)),
        (f64::NEG_INFINITY, Some(0)),
        (f64::NAN, None),
    ] {
        assert_eq!(convert_rating(rating), converted, "{rating}");
    }
}

#[test]
fn b20_harmonize_keeps_the_newest_row() {
    // The latest date wins over a higher count; no date is the oldest.
    let older = JfUserData {
        last_played: at(1),
        play_count: 9,
        position_ticks: 900,
        ..jf_row(1, 10)
    };
    let newer = JfUserData {
        last_played: at(2),
        play_count: 1,
        ..jf_row(1, 10)
    };
    let undated = JfUserData {
        play_count: 50,
        position_ticks: 5000,
        ..jf_row(1, 10)
    };
    let other_user = JfUserData {
        last_played: at(1),
        ..jf_row(2, 10)
    };
    let other_item = JfUserData {
        played: true,
        ..jf_row(1, 5)
    };
    let rows = harmonize(vec![
        older,
        undated.clone(),
        other_user.clone(),
        newer.clone(),
        other_item.clone(),
    ]);
    assert_eq!(
        rows,
        vec![other_item, newer, other_user],
        "one row per user and item, sorted by user, then item"
    );
    assert_eq!(harmonize(vec![undated.clone()]), vec![undated]);
    assert!(harmonize(Vec::new()).is_empty());
}

#[test]
fn b20_harmonize_breaks_ties_by_count_then_position() {
    let base = JfUserData {
        last_played: at(3),
        play_count: 2,
        position_ticks: 10,
        ..jf_row(1, 10)
    };
    let more_plays = JfUserData {
        play_count: 3,
        position_ticks: 5,
        ..base.clone()
    };
    let further = JfUserData {
        position_ticks: 20,
        ..base.clone()
    };
    let favorite = JfUserData {
        is_favorite: true,
        ..base.clone()
    };
    assert_eq!(
        harmonize(vec![base.clone(), more_plays.clone()]),
        vec![more_plays]
    );
    assert_eq!(
        harmonize(vec![further.clone(), base.clone()]),
        vec![further]
    );
    assert_eq!(
        harmonize(vec![base.clone(), favorite.clone()]),
        vec![base.clone()],
        "a full tie keeps the first"
    );
    assert_eq!(harmonize(vec![favorite.clone(), base]), vec![favorite]);
}

#[test]
fn b20_merge_without_a_rustyfin_row() {
    let expected = UserDataRow {
        played: true,
        play_count: 2,
        position_ticks: 600,
        last_played: at(5),
        is_favorite: true,
        user_rating: Some(8),
        likes: Some(true),
        audio: Some(2),
        audio_key: None,
        subtitle: Some(4),
        subtitle_key: None,
    };
    assert_eq!(
        merge(None, &jf_watched(), MAPPED),
        expected,
        "tracks are the mapped indexes"
    );
    // Without a Rustyfin row even an undated Jellyfin row is newer; unmapped tracks stay unset.
    let undated = JfUserData {
        position_ticks: 900,
        audio: Some(1),
        ..jf_row(1, 10)
    };
    let expected = UserDataRow {
        position_ticks: 900,
        ..UserDataRow::default()
    };
    assert_eq!(merge(None, &undated, MappedTracks::default()), expected);
}

#[test]
fn b20_merge_keeps_newer_rustyfin_progress() {
    let stored = UserDataRow {
        played: false,
        play_count: 5,
        position_ticks: 100,
        last_played: at(9),
        is_favorite: false,
        user_rating: Some(3),
        likes: Some(false),
        audio: Some(7),
        audio_key: None,
        subtitle: None,
        subtitle_key: None,
    };
    // Played and favorite if either; the larger count, the later date, Rustyfin's rating,
    // likes, position and tracks.
    let expected = UserDataRow {
        played: true,
        is_favorite: true,
        ..stored.clone()
    };
    assert_eq!(merge(Some(&stored), &jf_watched(), MAPPED), expected);
    let undated = JfUserData {
        last_played: None,
        ..jf_watched()
    };
    assert_eq!(
        merge(Some(&stored), &undated, MAPPED),
        expected,
        "an undated Jellyfin row is older than a dated Rustyfin row"
    );
}

#[test]
fn b20_merge_takes_newer_jellyfin_progress() {
    let stored = UserDataRow {
        play_count: 1,
        position_ticks: 50,
        last_played: at(1),
        audio: Some(7),
        subtitle: Some(0),
        ..UserDataRow::default()
    };
    // A track without a mapped value keeps Rustyfin's; subtitle -1 (off) is a value.
    let tracks = MappedTracks {
        audio: None,
        audio_key: None,
        subtitle: Some(-1),
        subtitle_key: None,
    };
    let expected = UserDataRow {
        played: true,
        play_count: 2,
        position_ticks: 600,
        last_played: at(5),
        is_favorite: true,
        user_rating: Some(8),
        likes: Some(true),
        audio: Some(7),
        audio_key: None,
        subtitle: Some(-1),
        subtitle_key: None,
    };
    assert_eq!(merge(Some(&stored), &jf_watched(), tracks), expected);

    // A Rustyfin row without a date is older than a dated Jellyfin row.
    let undated = UserDataRow {
        last_played: None,
        play_count: 4,
        ..stored
    };
    let merged = merge(Some(&undated), &jf_watched(), MAPPED);
    assert_eq!(
        (merged.position_ticks, merged.last_played, merged.play_count),
        (600, at(5), 4)
    );
    assert_eq!((merged.audio, merged.subtitle), (Some(2), Some(4)));
}

#[test]
fn b20_merge_on_a_tie_keeps_rustyfin_progress() {
    let stored = UserDataRow {
        position_ticks: 70,
        last_played: at(5),
        audio: Some(1),
        ..UserDataRow::default()
    };
    let expected = UserDataRow {
        played: true,
        play_count: 2,
        is_favorite: true,
        user_rating: Some(8),
        likes: Some(true),
        ..stored.clone()
    };
    assert_eq!(
        merge(Some(&stored), &jf_watched(), MAPPED),
        expected,
        "the same date"
    );

    let stored = UserDataRow {
        position_ticks: 70,
        ..UserDataRow::default()
    };
    let undated = JfUserData {
        position_ticks: 900,
        audio: Some(1),
        ..jf_row(1, 10)
    };
    let tracks = MappedTracks {
        audio: Some(2),
        audio_key: None,
        subtitle: None,
        subtitle_key: None,
    };
    assert_eq!(
        merge(Some(&stored), &undated, tracks),
        stored,
        "neither side has a date"
    );
}

#[test]
fn b20_merge_moves_keys_with_indexes() {
    let key = |name: &str| Some(name.to_string());
    let stored = UserDataRow {
        last_played: at(1),
        audio: Some(7),
        audio_key: key("rf audio"),
        subtitle: Some(0),
        subtitle_key: key("rf subtitle"),
        ..UserDataRow::default()
    };
    // A newer Jellyfin row: a mapped index brings its key, an unmapped one keeps Rustyfin's pair.
    let tracks = MappedTracks {
        audio: Some(2),
        audio_key: key("jf audio"),
        subtitle: None,
        subtitle_key: None,
    };
    let merged = merge(Some(&stored), &jf_watched(), tracks.clone());
    assert_eq!(
        (
            merged.audio,
            merged.audio_key,
            merged.subtitle,
            merged.subtitle_key
        ),
        (Some(2), key("jf audio"), Some(0), key("rf subtitle"))
    );
    // Subtitles off name no stream.
    let off = MappedTracks {
        subtitle: Some(-1),
        ..MappedTracks::default()
    };
    let merged = merge(Some(&stored), &jf_watched(), off);
    assert_eq!((merged.subtitle, merged.subtitle_key), (Some(-1), None));
    // An older Jellyfin row keeps both of Rustyfin's pairs.
    let newer = UserDataRow {
        last_played: at(9),
        ..stored.clone()
    };
    let merged = merge(Some(&newer), &jf_watched(), tracks);
    assert_eq!(
        (
            merged.audio,
            merged.audio_key,
            merged.subtitle,
            merged.subtitle_key
        ),
        (Some(7), key("rf audio"), Some(0), key("rf subtitle"))
    );
}

#[test]
fn b20_merge_ratings_and_likes() {
    let unrated = UserDataRow {
        last_played: at(9),
        ..UserDataRow::default()
    };
    let merged = merge(Some(&unrated), &jf_watched(), MAPPED);
    assert_eq!(
        (merged.user_rating, merged.likes),
        (Some(8), Some(true)),
        "Jellyfin's when Rustyfin has none"
    );
    let rated = UserDataRow {
        user_rating: Some(0),
        likes: Some(false),
        ..unrated.clone()
    };
    let merged = merge(Some(&rated), &jf_watched(), MAPPED);
    assert_eq!(
        (merged.user_rating, merged.likes),
        (Some(0), Some(false)),
        "Rustyfin's own when set"
    );
    let not_a_number = JfUserData {
        rating: Some(f64::NAN),
        likes: None,
        ..jf_watched()
    };
    let merged = merge(Some(&unrated), &not_a_number, MAPPED);
    assert_eq!((merged.user_rating, merged.likes), (None, None));
    let high = JfUserData {
        rating: Some(9.5),
        likes: Some(false),
        ..jf_watched()
    };
    let merged = merge(None, &high, MAPPED);
    assert_eq!((merged.user_rating, merged.likes), (Some(10), Some(false)));
}

#[test]
fn b20_merge_is_idempotent() {
    let stored = [
        None,
        Some(UserDataRow {
            play_count: 5,
            position_ticks: 100,
            last_played: at(9),
            user_rating: Some(3),
            audio: Some(7),
            ..UserDataRow::default()
        }),
        Some(UserDataRow {
            position_ticks: 50,
            last_played: at(1),
            subtitle: Some(0),
            ..UserDataRow::default()
        }),
        Some(UserDataRow {
            position_ticks: 70,
            last_played: at(5),
            ..UserDataRow::default()
        }),
        Some(UserDataRow {
            position_ticks: 70,
            ..UserDataRow::default()
        }),
    ];
    let undated = JfUserData {
        last_played: None,
        rating: None,
        ..jf_watched()
    };
    let off = MappedTracks {
        audio: None,
        audio_key: None,
        subtitle: Some(-1),
        subtitle_key: None,
    };
    for existing in &stored {
        for jf in [jf_watched(), undated.clone()] {
            for tracks in [MAPPED, MappedTracks::default(), off.clone()] {
                let once = merge(existing.as_ref(), &jf, tracks.clone());
                assert_eq!(
                    merge(Some(&once), &jf, tracks.clone()),
                    once,
                    "{existing:?} {jf:?} {tracks:?}"
                );
            }
        }
    }
}

/// A Rustyfin stream as the probe stores it: no language, title or flags.
fn rf_stream(index: i64, codec_type: &str, codec: &str) -> StreamRow {
    StreamRow {
        index,
        codec_type: codec_type.to_string(),
        codec: Some(codec.to_string()),
        language: None,
        title: None,
        is_external: false,
        path: None,
        is_default: false,
        is_forced: false,
    }
}

/// An embedded Jellyfin stream of item 10 without language, title or flags.
fn jf_stream(index: i64, kind: StreamKind, codec: &str) -> JfStream {
    JfStream {
        item: Uuid::from_u128(10),
        index,
        kind,
        codec: Some(codec.to_string()),
        language: None,
        title: None,
        is_external: false,
        path: None,
        is_default: false,
        is_forced: false,
    }
}

fn text(value: &str) -> Option<String> {
    Some(value.to_string())
}

#[test]
fn b20_choose_source() {
    let first = SourceStreams {
        path: "/m/A/a.mkv".to_string(),
        streams: vec![rf_stream(0, "video", "h264")],
    };
    let second = SourceStreams {
        path: "/m/A/b.mkv".to_string(),
        streams: Vec::new(),
    };
    let sources = [first.clone(), second.clone()];
    assert_eq!(choose_source(&sources, Some("/m/A/b.mkv")), Some(&second));
    assert_eq!(choose_source(&sources, Some("/m/A/a.mkv")), Some(&first));
    assert_eq!(
        choose_source(&sources, Some("/m/A/c.mkv")),
        None,
        "no source has the path: the choices were made on another file"
    );
    assert_eq!(choose_source(&sources, None), None, "no path: no source");
    assert_eq!(choose_source(&[], Some("/m/A/a.mkv")), None);
}

#[test]
fn b20_map_unique_tracks() {
    use StreamKind::{Audio, Subtitle, Video};
    let jf = [
        jf_stream(0, Video, "h264"),
        JfStream {
            language: text("eng"),
            ..jf_stream(1, Audio, "aac")
        },
        JfStream {
            language: text("ger"),
            ..jf_stream(2, Audio, "ac3")
        },
        JfStream {
            language: text("eng"),
            ..jf_stream(3, Subtitle, "srt")
        },
        JfStream {
            language: text("ger"),
            ..jf_stream(4, Subtitle, "PGSSUB")
        },
    ];
    // Rustyfin numbers the streams its own way and knows no languages.
    let rf = [
        rf_stream(0, "video", "h264"),
        rf_stream(1, "subtitle", "hdmv_pgs_subtitle"),
        rf_stream(2, "audio", "ac3"),
        rf_stream(3, "audio", "aac"),
        rf_stream(4, "subtitle", "subrip"),
    ];
    assert_eq!(map_track(1, Audio, &jf, &rf), Some(3));
    assert_eq!(map_track(2, Audio, &jf, &rf), Some(2));
    assert_eq!(map_track(3, Subtitle, &jf, &rf), Some(4), "srt = subrip");
    assert_eq!(map_track(4, Subtitle, &jf, &rf), Some(1), "pgssub");
    assert_eq!(map_track(9, Audio, &jf, &rf), None, "no Jellyfin stream 9");
    assert_eq!(map_track(0, Audio, &jf, &rf), None, "stream 0 is video");
    assert_eq!(map_track(1, Subtitle, &jf, &rf), None, "stream 1 is audio");
    assert_eq!(map_track(1, Audio, &jf, &[]), None, "no Rustyfin streams");
    let ac3_only = [rf_stream(2, "audio", "ac3")];
    assert_eq!(map_track(1, Audio, &jf, &ac3_only), None, "no aac stream");
    let upper = [rf_stream(5, "Audio", "AAC")];
    assert_eq!(
        map_track(1, Audio, &jf, &upper),
        Some(5),
        "codec types and codecs compare case-insensitively"
    );
}

#[test]
fn b20_map_tracks_by_ordinal() {
    use StreamKind::{Audio, Video};
    // Three AAC tracks Rustyfin can't tell apart: the ordinal among embedded audio decides.
    let jf = [
        jf_stream(0, Video, "h264"),
        JfStream {
            language: text("eng"),
            ..jf_stream(1, Audio, "aac")
        },
        JfStream {
            language: text("ger"),
            ..jf_stream(2, Audio, "aac")
        },
        JfStream {
            language: text("fre"),
            ..jf_stream(3, Audio, "aac")
        },
    ];
    let rf = [
        rf_stream(0, "video", "h264"),
        rf_stream(1, "audio", "aac"),
        rf_stream(2, "audio", "aac"),
        rf_stream(3, "audio", "aac"),
    ];
    assert_eq!(map_track(1, Audio, &jf, &rf), Some(1));
    assert_eq!(map_track(2, Audio, &jf, &rf), Some(2));
    assert_eq!(map_track(3, Audio, &jf, &rf), Some(3));

    let jf = [
        jf_stream(1, Audio, "aac"),
        jf_stream(2, Audio, "ac3"),
        jf_stream(3, Audio, "aac"),
    ];
    let rf = [
        rf_stream(1, "audio", "aac"),
        rf_stream(2, "audio", "aac"),
        rf_stream(3, "audio", "ac3"),
    ];
    assert_eq!(map_track(1, Audio, &jf, &rf), Some(1));
    assert_eq!(
        map_track(3, Audio, &jf, &rf),
        None,
        "the stream at the ordinal is no candidate"
    );
    assert_eq!(map_track(2, Audio, &jf, &rf), Some(3), "one candidate");
    let two = [rf_stream(1, "audio", "aac"), rf_stream(2, "audio", "aac")];
    assert_eq!(
        map_track(3, Audio, &jf, &two),
        None,
        "no stream at the ordinal"
    );
}

#[test]
fn b20_map_tracks_by_flags_language_and_title() {
    use StreamKind::{Audio, Subtitle};
    let jf = [
        JfStream {
            is_forced: true,
            ..jf_stream(3, Subtitle, "subrip")
        },
        JfStream {
            is_default: true,
            ..jf_stream(4, Subtitle, "subrip")
        },
        jf_stream(5, Subtitle, "subrip"),
    ];
    let rf = [
        rf_stream(3, "subtitle", "subrip"),
        StreamRow {
            is_default: true,
            ..rf_stream(4, "subtitle", "subrip")
        },
        StreamRow {
            is_forced: true,
            ..rf_stream(5, "subtitle", "subrip")
        },
    ];
    assert_eq!(map_track(3, Subtitle, &jf, &rf), Some(5), "forced");
    assert_eq!(map_track(4, Subtitle, &jf, &rf), Some(4), "default");
    assert_eq!(map_track(5, Subtitle, &jf, &rf), Some(3), "neither");
    let unflagged = [rf_stream(3, "subtitle", "subrip")];
    assert_eq!(
        map_track(3, Subtitle, &jf, &unflagged),
        None,
        "no forced stream"
    );

    // Languages and titles narrow the candidates when both sides know them.
    let jf = [
        JfStream {
            language: text("ENG"),
            ..jf_stream(1, Audio, "aac")
        },
        JfStream {
            language: text("ger"),
            title: text("Commentary"),
            ..jf_stream(2, Audio, "aac")
        },
        JfStream {
            language: text("ger"),
            title: text("Stereo"),
            ..jf_stream(3, Audio, "aac")
        },
    ];
    let rf = [
        StreamRow {
            language: text("ger"),
            title: text("stereo"),
            ..rf_stream(1, "audio", "aac")
        },
        StreamRow {
            language: text("eng"),
            ..rf_stream(2, "audio", "aac")
        },
        StreamRow {
            language: text("ger"),
            title: text("commentary"),
            ..rf_stream(3, "audio", "aac")
        },
    ];
    assert_eq!(map_track(1, Audio, &jf, &rf), Some(2), "language");
    assert_eq!(map_track(2, Audio, &jf, &rf), Some(3), "title");
    assert_eq!(map_track(3, Audio, &jf, &rf), Some(1), "language and title");

    // An unknown language (none, empty or `und`) rules nothing out.
    let jf = [
        JfStream {
            language: text("und"),
            ..jf_stream(1, Audio, "aac")
        },
        JfStream {
            language: text("eng"),
            ..jf_stream(2, Audio, "aac")
        },
    ];
    let rf = [
        StreamRow {
            language: text("ger"),
            ..rf_stream(1, "audio", "aac")
        },
        StreamRow {
            language: text(""),
            ..rf_stream(2, "audio", "aac")
        },
    ];
    assert_eq!(
        map_track(1, Audio, &jf, &rf),
        Some(1),
        "two candidates, ordinal 0"
    );
    assert_eq!(
        map_track(2, Audio, &jf, &rf),
        Some(2),
        "eng rules out ger only"
    );
}

#[test]
fn b20_map_external_tracks() {
    use StreamKind::{Audio, Subtitle, Video};
    let jf_ext = |index: i64, path: &str| JfStream {
        is_external: true,
        path: text(path),
        ..jf_stream(index, Subtitle, "srt")
    };
    // Jellyfin numbers external streams first; the slice need not be sorted.
    let jf = [
        jf_stream(2, Video, "h264"),
        jf_stream(3, Audio, "aac"),
        JfStream {
            language: text("ger"),
            ..jf_stream(5, Subtitle, "subrip")
        },
        JfStream {
            language: text("eng"),
            ..jf_stream(4, Subtitle, "subrip")
        },
        jf_ext(0, "/data/movies/Alpha (2001)/Alpha.en.srt"),
        jf_ext(1, "/data/movies/Alpha (2001)/Alpha.de.srt"),
    ];
    let rf_ext = |index: i64, path: &str| StreamRow {
        is_external: true,
        path: text(path),
        ..rf_stream(index, "subtitle", "subrip")
    };
    let rf = [
        rf_stream(0, "video", "h264"),
        rf_stream(1, "audio", "aac"),
        rf_stream(2, "subtitle", "subrip"),
        rf_stream(3, "subtitle", "subrip"),
        rf_ext(4, "/srv/movies/Alpha (2001)/Alpha.de.srt"),
    ];
    assert_eq!(
        map_track(4, Subtitle, &jf, &rf),
        Some(2),
        "embedded ordinal 0"
    );
    assert_eq!(
        map_track(5, Subtitle, &jf, &rf),
        Some(3),
        "embedded ordinal 1"
    );
    assert_eq!(map_track(3, Audio, &jf, &rf), Some(1));
    assert_eq!(map_track(1, Subtitle, &jf, &rf), Some(4), "by file name");
    assert_eq!(map_track(0, Subtitle, &jf, &rf), None, "no Alpha.en.srt");
    let twice = [
        rf_ext(4, "/srv/a/Alpha.de.srt"),
        rf_ext(5, "/srv/b/Alpha.de.srt"),
    ];
    assert_eq!(
        map_track(1, Subtitle, &jf, &twice),
        None,
        "several external candidates"
    );
    let other_case = [rf_ext(4, "/srv/movies/Alpha (2001)/ALPHA.de.srt")];
    assert_eq!(
        map_track(1, Subtitle, &jf, &other_case),
        None,
        "file names compare exactly"
    );
}

// 5.8: the old layout's `Images` text: `Path*Ticks*Type[*Width*Height[*Blurhash]]` entries joined
// by `|`, with `/` and `\` standing for `*` and `|` in a blurhash. Sizes of 0 or less and an empty
// blurhash are none; malformed entries and ignored or unknown types are skipped.
#[test]
fn b20_old_layout_image_entries() {
    let text = "/m/poster.jpg*637000000000000000*Primary*1000*1500*LEHV/nWB\\yk8\
                |%MetadataPath%/b.jpg*1*Backdrop\
                |/m/box.jpg*1*Box*10*10*x\
                |/m/s.png*1*Screenshot*0*-1*\
                |garbage\
                |/m/odd.jpg*1*NoSuchType*1*1*h";
    let image =
        |path: &str, image_type, size: Option<(i32, i32)>, blurhash: Option<&str>| JfImage {
            path: path.to_string(),
            image_type,
            width: size.map(|s| s.0),
            height: size.map(|s| s.1),
            blurhash: blurhash.map(str::to_string),
        };
    assert_eq!(
        parse_old_images(text),
        vec![
            image(
                "/m/poster.jpg",
                ImageType::Primary,
                Some((1000, 1500)),
                Some("LEHV*nWB|yk8")
            ),
            image("%MetadataPath%/b.jpg", ImageType::Backdrop, None, None),
            image("/m/s.png", ImageType::Screenshot, None, None),
        ]
    );
    assert_eq!(parse_old_images(""), Vec::new());
}

// 5.9: a copy's type comes from the file's first bytes; anything else is unusable.
#[test]
fn b20_image_signatures() {
    assert_eq!(
        signature(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]),
        Some("jpg")
    );
    assert_eq!(signature(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"), Some("png"));
    assert_eq!(signature(b"RIFF\x24\0\0\0WEBPVP8 "), Some("webp"));
    assert_eq!(signature(b"RIFF\0\0\0\0WEBP"), Some("webp"));
    assert_eq!(signature(b"GIF87a\x01\0"), Some("gif"));
    assert_eq!(signature(b"GIF89a\x01\0"), Some("gif"));
    let others: [&[u8]; 7] = [
        b"SQLite format 3\0",
        b"RIFF\x24\0\0\0WAVEfmt ",
        b"RIFF\0\0\0\0WEB",
        b"GIF88a",
        &[0xFF, 0xD8],
        b"<html>",
        b"",
    ];
    for other in others {
        assert_eq!(signature(other), None, "{other:?}");
    }
}

// 5.9: image paths resolve through the pairs (root: the pair's local side), else the placeholders
// under Jellyfin's program data; `..`, relative paths and URLs are missing.
#[test]
fn b20_resolve_image_paths() {
    let program_data = Path::new("/srv/jf");
    let pairs = vec![
        pair("/config/metadata", "/srv/jf/metadata"),
        pair("/data/movies", "/media/movies"),
    ];
    let resolved = |path: &str| resolve(path, program_data, &pairs);
    let found = |file: &str, root: &str| Some((PathBuf::from(file), PathBuf::from(root)));
    assert_eq!(
        resolved("%MetadataPath%/library/ab/x/poster.jpg"),
        found(
            "/srv/jf/metadata/library/ab/x/poster.jpg",
            "/srv/jf/metadata"
        )
    );
    assert_eq!(
        resolved("%AppDataPath%/collections/c/poster.jpg"),
        found("/srv/jf/data/collections/c/poster.jpg", "/srv/jf/data")
    );
    assert_eq!(
        resolved("/config/metadata/People/A/Ann/folder.jpg"),
        found(
            "/srv/jf/metadata/People/A/Ann/folder.jpg",
            "/srv/jf/metadata"
        )
    );
    assert_eq!(
        resolved("/data/movies/Alpha (2001)/logo.png"),
        found("/media/movies/Alpha (2001)/logo.png", "/media/movies")
    );
    for missing in [
        "/elsewhere/x.jpg",
        "%MetadataPath%/../data/jellyfin.db",
        "/config/metadata/../outside.jpg",
        "/data/movies/../../etc/passwd",
        "https://image.example/a.jpg",
        "metadata/library/x.jpg",
        "",
    ] {
        assert_eq!(resolved(missing), None, "{missing}");
    }

    // A pair for a placeholder wins over program data.
    let moved = vec![pair("%MetadataPath%", "/mnt/jfmeta")];
    assert_eq!(
        resolve("%MetadataPath%/library/x.jpg", program_data, &moved),
        found("/mnt/jfmeta/library/x.jpg", "/mnt/jfmeta")
    );
}

// 5.9: the suggested pair for Jellyfin's metadata folder: the prefix up to a `metadata` component
// that most paths share (ties: shortest, then smallest), only when `<program data>/metadata`
// exists and no pair covers the prefix.
#[test]
fn b20_suggest_metadata_pair() {
    let tmp = TempDir::new();
    let paths = [
        "/config/metadata/library/ab/x/poster.jpg",
        "/config/metadata/People/A/Ann/folder.jpg",
        "/other/metadata/library/x.jpg",
        "%MetadataPath%/library/y.jpg",
        "/data/movies/A/poster.jpg",
    ];
    assert_eq!(
        suggest_metadata(&paths, &tmp.0, &[]),
        None,
        "no local metadata folder"
    );
    let local = tmp.0.join("metadata");
    std::fs::create_dir_all(&local).unwrap();
    let local = local.display().to_string();
    assert_eq!(
        suggest_metadata(&paths, &tmp.0, &[]),
        Some(pair("/config/metadata", &local))
    );
    let covered = [pair("/config", "/srv/config")];
    assert_eq!(
        suggest_metadata(&paths, &tmp.0, &covered),
        None,
        "a pair covers the prefix"
    );
    let tie = [
        "/b/metadata/x.jpg",
        "/a/metadata/y.jpg",
        "/long/er/metadata/z.jpg",
    ];
    assert_eq!(
        suggest_metadata(&tie, &tmp.0, &[]),
        Some(pair("/a/metadata", &local))
    );
    assert_eq!(suggest_metadata(&[], &tmp.0, &[]), None);
}

// Fix 7, 8: a source database is read into memory with its `-wal` applied as SQLite's recovery
// would: committed frames only, up to the first torn or damaged frame; a damaged header means no
// frames. Reading changes nothing in the folder.

/// A WAL-mode `jellyfin.db` in `dir` whose file holds table `t` with row 1, and a writer that
/// never checkpoints, so what it adds stays in the `-wal`.
async fn wal_writer(dir: &Path) -> sqlx::SqliteConnection {
    use sqlx::ConnectOptions;
    let mut conn = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.join("jellyfin.db"))
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .connect()
        .await
        .unwrap();
    sqlx::raw_sql(
        "CREATE TABLE t (n INTEGER, pad TEXT); INSERT INTO t VALUES (1, ''); \
         PRAGMA wal_checkpoint(TRUNCATE); PRAGMA wal_autocheckpoint = 0;",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    conn
}

/// The `n` of every row of `t` in the snapshot of `file`.
async fn snapshot_rows(file: &Path) -> Vec<i64> {
    let mut conn = open_snapshot(file)
        .await
        .unwrap_or_else(|e| panic!("{}: {e:?}", file.display()));
    sqlx::query_scalar("SELECT n FROM t ORDER BY n")
        .fetch_all(&mut conn)
        .await
        .unwrap()
}

/// Each entry of `dir` with its size and modification time.
fn folder_listing(dir: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            (entry.path(), meta.len(), meta.modified().unwrap())
        })
        .collect();
    entries.sort();
    entries
}

/// A copy of `from`'s `jellyfin.db` with `wal` as its `-wal`, in the new folder `to`.
fn source_with_wal(from: &Path, to: &Path, wal: &[u8]) -> PathBuf {
    std::fs::create_dir_all(to).unwrap();
    let file = to.join("jellyfin.db");
    std::fs::copy(from.join("jellyfin.db"), &file).unwrap();
    std::fs::write(to.join("jellyfin.db-wal"), wal).unwrap();
    file
}

#[tokio::test]
async fn b20_snapshot_applies_committed_wal_frames() {
    let tmp = TempDir::new();
    let mut writer = wal_writer(&tmp.0).await;
    sqlx::query("INSERT INTO t VALUES (2, '')")
        .execute(&mut writer)
        .await
        .unwrap();
    let file = tmp.0.join("jellyfin.db");
    let before = folder_listing(&tmp.0);
    assert_eq!(snapshot_rows(&file).await, [1, 2]);
    assert_eq!(folder_listing(&tmp.0), before, "reading changed the folder");
    let no_wal = source_with_wal(&tmp.0, &tmp.0.join("no_wal"), b"");
    assert_eq!(
        snapshot_rows(&no_wal).await,
        [1],
        "row 2 is only in the -wal"
    );
}

#[tokio::test]
async fn b20_snapshot_skips_torn_and_uncommitted_frames() {
    let tmp = TempDir::new();
    let mut writer = wal_writer(&tmp.0).await;
    sqlx::query("INSERT INTO t VALUES (2, '')")
        .execute(&mut writer)
        .await
        .unwrap();
    let wal_path = tmp.0.join("jellyfin.db-wal");
    let first = std::fs::metadata(&wal_path).unwrap().len() as usize;
    // Twenty rows of 4000 characters: one transaction over many pages, so many frames.
    sqlx::query(
        "WITH RECURSIVE k(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM k WHERE i < 20) \
         INSERT INTO t SELECT 3, hex(randomblob(2000)) FROM k",
    )
    .execute(&mut writer)
    .await
    .unwrap();
    let wal = std::fs::read(&wal_path).unwrap();
    let page_size = u32::from_be_bytes(wal[8..12].try_into().unwrap()) as usize;
    let frame = 24 + page_size;
    assert!(
        wal.len() >= first + 3 * frame,
        "the second transaction spans frames"
    );
    for (case, cut) in [
        ("whole first transaction", first),
        ("frames without their commit", first + 2 * frame + 100),
        ("torn commit frame", wal.len() - 1),
    ] {
        let dir = tmp.0.join(case.replace(' ', "_"));
        let file = source_with_wal(&tmp.0, &dir, &wal[..cut]);
        assert_eq!(snapshot_rows(&file).await, [1, 2], "{case}");
    }
    let all = snapshot_rows(&tmp.0.join("jellyfin.db")).await;
    assert_eq!(all.len(), 22, "the whole -wal");
    assert_eq!(all.iter().filter(|&&n| n == 3).count(), 20);
}

#[tokio::test]
async fn b20_snapshot_stops_at_a_damaged_frame_and_ignores_a_damaged_header() {
    let tmp = TempDir::new();
    let mut writer = wal_writer(&tmp.0).await;
    let wal_path = tmp.0.join("jellyfin.db-wal");
    let mut ends = Vec::new();
    for n in 2i64..=4 {
        sqlx::query("INSERT INTO t VALUES (?, '')")
            .bind(n)
            .execute(&mut writer)
            .await
            .unwrap();
        ends.push(std::fs::metadata(&wal_path).unwrap().len() as usize);
    }
    let wal = std::fs::read(&wal_path).unwrap();
    // A byte of the page in the first frame that adds 3: 3 and 4 are gone.
    let mut bad_page = wal.clone();
    bad_page[ends[0] + 24 + 100] ^= 0xff;
    // A salt of the first frame that adds 4: only 4 is gone.
    let mut bad_salt = wal.clone();
    bad_salt[ends[1] + 8] ^= 0xff;
    // A salt in the header: its checksum fails, so no frame counts.
    let mut bad_header = wal.clone();
    bad_header[16] ^= 0xff;
    for (case, bytes, rows) in [
        ("damaged page", bad_page, vec![1, 2]),
        ("damaged frame salt", bad_salt, vec![1, 2, 3]),
        ("damaged header", bad_header, vec![1]),
    ] {
        let dir = tmp.0.join(case.replace(' ', "_"));
        let file = source_with_wal(&tmp.0, &dir, &bytes);
        assert_eq!(snapshot_rows(&file).await, rows, "{case}");
    }
    assert_eq!(
        snapshot_rows(&tmp.0.join("jellyfin.db")).await,
        [1, 2, 3, 4]
    );
}

/// Writes a JPEG of `len` bytes to `path`: the signature, then `fill`.
fn write_jpeg(path: &Path, len: usize, fill: u8) {
    let mut bytes = vec![fill; len];
    bytes[..3].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
    std::fs::write(path, bytes).unwrap();
}

#[cfg(unix)]
#[test]
fn b20_image_copy_takes_only_the_checked_file() {
    use super::images::{check, copy};
    use std::os::unix::fs::symlink;
    let tmp = TempDir::new();
    let root = tmp.0.join("metadata");
    let outside = tmp.0.join("outside");
    let out = tmp.0.join("images");
    for dir in [root.join("aa"), outside.join("aa")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    // A file outside the root that looks like the poster: a JPEG of the same length and time.
    let poster = root.join("aa/poster.jpg");
    let secret = outside.join("aa/poster.jpg");
    write_jpeg(&poster, 4096, b'p');
    write_jpeg(&secret, 4096, b's');
    let modified = std::fs::metadata(&poster).unwrap().modified().unwrap();
    let file = std::fs::File::options().write(true).open(&secret).unwrap();
    file.set_modified(modified).unwrap();
    drop(file);

    // Unchanged, the checked file is copied.
    let image = check(&poster, &root).unwrap();
    copy(&image, &out.join("kept.jpg")).unwrap();
    assert_eq!(
        std::fs::read(out.join("kept.jpg")).unwrap(),
        std::fs::read(&poster).unwrap()
    );

    // After its check, the file becomes a link to the other one.
    let image = check(&poster, &root).unwrap();
    let moved = root.join("aa/poster.moved");
    std::fs::rename(&poster, &moved).unwrap();
    symlink(&secret, &poster).unwrap();
    let copied = copy(&image, &out.join("link.jpg"));
    assert!(copied.is_err(), "copied the file a link put in its place");
    std::fs::remove_file(&poster).unwrap();
    std::fs::rename(&moved, &poster).unwrap();

    // After its check, its folder becomes a link to a folder outside the root.
    let image = check(&poster, &root).unwrap();
    std::fs::rename(root.join("aa"), root.join("aa.moved")).unwrap();
    symlink(outside.join("aa"), root.join("aa")).unwrap();
    let copied = copy(&image, &out.join("folder.jpg"));
    assert!(copied.is_err(), "copied through a folder link");
    std::fs::remove_file(root.join("aa")).unwrap();
    std::fs::rename(root.join("aa.moved"), root.join("aa")).unwrap();

    // After its check, the file grows.
    let image = check(&poster, &root).unwrap();
    let mut file = std::fs::File::options().append(true).open(&poster).unwrap();
    std::io::Write::write_all(&mut file, &[b'p'; 100]).unwrap();
    drop(file);
    let copied = copy(&image, &out.join("grown.jpg"));
    assert!(copied.is_err(), "copied a file that changed");

    // Only the first copy is there, and no temporary file.
    let names: Vec<_> = std::fs::read_dir(&out)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["kept.jpg"]);
}
