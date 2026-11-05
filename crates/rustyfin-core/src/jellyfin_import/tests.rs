//! B20: unit tests for the Jellyfin importer's pure parts (written with the spec, before the code).

use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};
use uuid::Uuid;

use super::ImportError;
use super::paths::{PathMapping, translate_path};
use super::plan::{UserMatch, match_user_name};
use super::source::{
    ProviderIds, guid_from_blob, guid_from_text, locate, parse_jellyfin_date, parse_provider_ids,
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
