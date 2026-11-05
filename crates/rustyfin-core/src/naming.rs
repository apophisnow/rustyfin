//! Folder and file names that the library scanner skips.
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
