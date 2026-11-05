//! Names the scanner reads: folders and files it skips, and episode numbers in file names.
//!
//! The rules follow Jellyfin's `IgnorePatterns.cs` (commit 06695fb4942c15c7463c2dc644440d988951ed17).
//! The functions are pure, with no I/O. Names are compared case-insensitively. The scanner applies
//! them only to entries below a library root, and only to the entry's own name (for a symlink,
//! the link's name).

/// Folder names that are skipped whole, in lower case.
const IGNORED_DIR_NAMES: &[&str] = &[
    "metadata",
    "ps3_update",
    "ps3_vprm",
    "extrafanart",
    "extrathumbs",
    "lost+found",
    "subs",
    "temprec",
    "tempsbe",
    "eadir",
    "#recycle",
    "$recycle.bin",
    "system volume information",
    "sample",
    "minta",
];

/// File names that are skipped whole, in lower case.
const IGNORED_FILE_NAMES: &[&str] = &["small.jpg", "albumart.jpg", "thumbs.db"];

/// File name endings that are skipped, in lower case.
const IGNORED_FILE_SUFFIXES: &[&str] = &[".bts", ".sync", ".trickplay"];

/// Whether the scanner must not enter the folder called `name`.
///
/// Skipped: names that start with `.` or `@`, names in `IGNORED_DIR_NAMES`, and names that end
/// with `.trickplay`.
pub fn is_ignored_dir(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.starts_with(['.', '@'])
        || IGNORED_DIR_NAMES.contains(&lower.as_str())
        || lower.ends_with(".trickplay")
}

/// Whether the scanner must not collect the file called `name`.
///
/// Skipped: names that start with `.`, `small.jpg`, `albumart.jpg` and `thumbs.db`, names that end
/// with `.bts`, `.sync` or `.trickplay`, and `sample.X`, `*.sample.X`, `minta.X` and `*.minta.X`,
/// where X is 1 to 5 characters with no dot.
pub fn is_ignored_file(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.starts_with('.')
        || IGNORED_FILE_NAMES.contains(&lower.as_str())
        || IGNORED_FILE_SUFFIXES
            .iter()
            .any(|&suffix| lower.ends_with(suffix))
        || is_sample_file(&lower, "sample")
        || is_sample_file(&lower, "minta")
}

/// Whether the lower-case file name is `word.X` or `*.word.X`, where X is as `is_extension` allows.
fn is_sample_file(lower: &str, word: &str) -> bool {
    // `word.X`: the whole name.
    let whole = lower
        .strip_prefix(word)
        .and_then(|rest| rest.strip_prefix('.'))
        .is_some_and(is_extension);
    // `*.word.X`: X has no dot, so the last `.word.` is the one that starts X.
    let marker = format!(".{word}.");
    let suffixed = lower
        .rfind(marker.as_str())
        .is_some_and(|at| is_extension(&lower[at + marker.len()..]));
    whole || suffixed
}

/// Whether `ext` is 1 to 5 characters long and contains no dot.
fn is_extension(ext: &str) -> bool {
    (1..=5).contains(&ext.chars().count()) && !ext.contains('.')
}

/// The season and episode numbers that `parse_episode` reads from a file name.
///
/// `season` is `None` when the name gives only an episode. `ending` is the last episode of a
/// multi-episode file, such as 3 in `Show S01E02E03`, when it counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpisodeNumbers {
    pub season: Option<i32>,
    pub episode: i32,
    pub ending: Option<i32>,
}

/// One episode rule: it reads the numbers from a stem's bytes, if the stem matches it.
type EpisodeRule = fn(&[u8]) -> Option<EpisodeNumbers>;

/// Reads the season and episode numbers from a file name stem. Returns `None` when no rule gives
/// a valid result.
///
/// The rules follow Jellyfin's `EpisodePathParser.cs` and `NamingOptions.cs` (commit
/// 06695fb4942c15c7463c2dc644440d988951ed17). Matching is ASCII and case-insensitive. The rules
/// are tried in order, and within a rule the leftmost match is its result:
///
/// 1. `S01E02`: `s`, digits, any run of `[ ] space . _ -`, `e`, digits. Season and episode.
/// 2. `ep05`: one of `. _ space -`, `ep`, an optional `_`, digits. Episode only.
/// 3. `1x02`: at the start or after one of `. _ space [ ( -`, 1 to 4 digits, `x`, 1 to 3 digits.
///    Season and episode.
/// 4. `Episode 7`: `episode`, optional spaces, digits, and optionally `-M` as the ending. Episode
///    only.
///
/// A rule whose season is invalid gives nothing (see `is_valid_season`). Rules 1 and 3 also read
/// a multi-episode ending (see `repeated_ending`), and rule 4 reads `-M`.
pub fn parse_episode(stem: &str) -> Option<EpisodeNumbers> {
    let bytes = stem.as_bytes();
    let rules: [EpisodeRule; 4] = [rule_season_episode, rule_ep, rule_nxnn, rule_episode_word];
    rules
        .into_iter()
        .find_map(|rule| rule(bytes).filter(|found| found.season.is_none_or(is_valid_season)))
}

/// Rule 1, `S01E02`: `s`, 1+ digits, any run of `[ ] space . _ -`, `e`, 1+ digits.
fn rule_season_episode(bytes: &[u8]) -> Option<EpisodeNumbers> {
    (0..bytes.len()).find_map(|at| {
        if !is_ci_at(bytes, at, b's') {
            return None;
        }
        let season_end = digits_end(bytes, at + 1);
        let season = parse_digits(&bytes[at + 1..season_end])?;
        let mut e_at = season_end;
        while bytes.get(e_at).is_some_and(|b| b"[] ._-".contains(b)) {
            e_at += 1;
        }
        if !is_ci_at(bytes, e_at, b'e') {
            return None;
        }
        let episode_end = digits_end(bytes, e_at + 1);
        let episode = parse_digits(&bytes[e_at + 1..episode_end])?;
        Some(EpisodeNumbers {
            season: Some(season),
            episode,
            ending: repeated_ending(bytes, episode_end, episode),
        })
    })
}

/// Rule 2, `ep05`: one of `. _ space -`, then `ep`, an optional `_`, then 1+ digits.
fn rule_ep(bytes: &[u8]) -> Option<EpisodeNumbers> {
    (0..bytes.len()).find_map(|at| {
        if !matches!(bytes[at], b'.' | b'_' | b' ' | b'-')
            || !(is_ci_at(bytes, at + 1, b'e') && is_ci_at(bytes, at + 2, b'p'))
        {
            return None;
        }
        let mut start = at + 3;
        if bytes.get(start) == Some(&b'_') {
            start += 1;
        }
        let episode = parse_digits(&bytes[start..digits_end(bytes, start)])?;
        Some(EpisodeNumbers {
            season: None,
            episode,
            ending: None,
        })
    })
}

/// Rule 3, `1x02`: at the start or after one of `. _ space [ ( -`, 1 to 4 digits, `x`, 1 to 3
/// digits. The digit runs are whole: `1x1080` is no match, since 1080 has four digits.
fn rule_nxnn(bytes: &[u8]) -> Option<EpisodeNumbers> {
    (0..bytes.len()).find_map(|at| {
        let starts_number = at == 0 || b"._ [(-".contains(&bytes[at - 1]);
        if !starts_number || !bytes[at].is_ascii_digit() {
            return None;
        }
        let season_end = digits_end(bytes, at);
        if !is_ci_at(bytes, season_end, b'x') || !(1..=4).contains(&(season_end - at)) {
            return None;
        }
        let episode_start = season_end + 1;
        let episode_end = digits_end(bytes, episode_start);
        if !(1..=3).contains(&(episode_end - episode_start)) {
            return None;
        }
        let season = parse_digits(&bytes[at..season_end])?;
        let episode = parse_digits(&bytes[episode_start..episode_end])?;
        Some(EpisodeNumbers {
            season: Some(season),
            episode,
            ending: repeated_ending(bytes, episode_end, episode),
        })
    })
}

/// Rule 4, `Episode 7`: `episode`, optional spaces, 1+ digits, and optionally `-M` as the ending.
fn rule_episode_word(bytes: &[u8]) -> Option<EpisodeNumbers> {
    (0..bytes.len()).find_map(|at| {
        if !bytes.get(at..at + 7)?.eq_ignore_ascii_case(b"episode") {
            return None;
        }
        let mut start = at + 7;
        while bytes.get(start) == Some(&b' ') {
            start += 1;
        }
        let end = digits_end(bytes, start);
        let episode = parse_digits(&bytes[start..end])?;
        let ending = if bytes.get(end) == Some(&b'-') {
            let ending_end = digits_end(bytes, end + 1);
            parse_digits(&bytes[end + 1..ending_end])
                .filter(|&number| ending_counts(bytes, ending_end, number, episode))
        } else {
            None
        };
        Some(EpisodeNumbers {
            season: None,
            episode,
            ending,
        })
    })
}

/// Whether a season can be one. Jellyfin rejects 200 to 1927, which read as years in names such as
/// `1920x1080`, and anything above 2500 (`EpisodePathParser.cs` 188-192).
fn is_valid_season(season: i32) -> bool {
    !(200..1928).contains(&season) && season <= 2500
}

/// The multi-episode ending after the episode digits at `pos` (rules 1 and 3), if it counts.
///
/// The forms repeat, and the last one matched gives the number, so `S01E01E02E03` ends at 3. The
/// check in `ending_counts` applies to that last form only.
fn repeated_ending(bytes: &[u8], mut pos: usize, episode: i32) -> Option<i32> {
    let mut last = None;
    while let Some((number, end)) = ending_form(bytes, pos) {
        last = Some((number, end));
        pos = end;
    }
    let (number, end) = last?;
    ending_counts(bytes, end, number, episode).then_some(number)
}

/// One ending form at `pos`, as its number and the index after its digits.
fn ending_form(bytes: &[u8], pos: usize) -> Option<(i32, usize)> {
    // Form (c) is tried first, as the first multi-episode expressions in Jellyfin's
    // `NamingOptions.cs` (773-776) are. So `-1234e05` gives 5, where form (b) would stop at `-123`.
    form_c(bytes, pos)
        .or_else(|| form_a(bytes, pos))
        .or_else(|| form_b(bytes, pos))
}

/// Form (a): an optional `-` or ` - `, then `x` or `e`, then 1 to 3 digits.
fn form_a(bytes: &[u8], pos: usize) -> Option<(i32, usize)> {
    let x_at = dash_end(bytes, pos).unwrap_or(pos);
    if !(is_ci_at(bytes, x_at, b'x') || is_ci_at(bytes, x_at, b'e')) {
        return None;
    }
    ending_digits(bytes, x_at + 1)
}

/// Form (b): `-`, an optional `x`, an optional `e`, then 1 to 3 digits.
fn form_b(bytes: &[u8], pos: usize) -> Option<(i32, usize)> {
    if bytes.get(pos) != Some(&b'-') {
        return None;
    }
    let mut at = pos + 1;
    if is_ci_at(bytes, at, b'x') {
        at += 1;
    }
    if is_ci_at(bytes, at, b'e') {
        at += 1;
    }
    ending_digits(bytes, at)
}

/// Form (c): `-` or ` - `, then 1 to 4 digits, then `e`, `x` or `xe`, then 1 to 3 digits.
fn form_c(bytes: &[u8], pos: usize) -> Option<(i32, usize)> {
    let start = dash_end(bytes, pos)?;
    let digits = digits_end(bytes, start);
    if !(1..=4).contains(&(digits - start)) {
        return None;
    }
    let at = if is_ci_at(bytes, digits, b'x') && is_ci_at(bytes, digits + 1, b'e') {
        digits + 2
    } else if is_ci_at(bytes, digits, b'x') || is_ci_at(bytes, digits, b'e') {
        digits + 1
    } else {
        return None;
    };
    ending_digits(bytes, at)
}

/// Up to 3 digits from `from`, as their number and the index after them. A longer run is cut after
/// 3 here, and `ending_counts` then rejects the ending, since a digit follows.
fn ending_digits(bytes: &[u8], from: usize) -> Option<(i32, usize)> {
    let end = digits_end(bytes, from).min(from + 3);
    Some((parse_digits(&bytes[from..end])?, end))
}

/// Whether an ending counts (`EpisodePathParser.cs` 151-167). The character at `end`, just after
/// its digits, must not be a digit or one of `iIpP` (as in `-1080p`), and the number must be at
/// least the episode.
fn ending_counts(bytes: &[u8], end: usize, number: i32, episode: i32) -> bool {
    let continues = bytes
        .get(end)
        .is_some_and(|b| b.is_ascii_digit() || b"iIpP".contains(b));
    !continues && number >= episode
}

/// The index after a `-` or ` - ` at `pos`, if there is one.
fn dash_end(bytes: &[u8], pos: usize) -> Option<usize> {
    if bytes
        .get(pos..)
        .is_some_and(|rest| rest.starts_with(b" - "))
    {
        Some(pos + 3)
    } else if bytes.get(pos) == Some(&b'-') {
        Some(pos + 1)
    } else {
        None
    }
}

/// The index just past the run of ASCII digits that starts at `from`.
fn digits_end(bytes: &[u8], mut from: usize) -> usize {
    while bytes.get(from).is_some_and(u8::is_ascii_digit) {
        from += 1;
    }
    from
}

/// The number that `digits` spells, or `None` if it is empty or too long for an `i32`.
fn parse_digits(digits: &[u8]) -> Option<i32> {
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// Whether the byte at `at` is `c`, ignoring ASCII case. Non-ASCII bytes never match.
fn is_ci_at(bytes: &[u8], at: usize, c: u8) -> bool {
    bytes.get(at).is_some_and(|b| b.eq_ignore_ascii_case(&c))
}
