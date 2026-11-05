//! Path pairs (spec 2.8): translation by the longest component prefix, and the suggested pairs
//! from Jellyfin's library folders.

use std::path::Path;

use rustyfin_model::library::Library;
use serde::{Deserialize, Serialize};

use super::source::LibraryFolder;

/// One path pair: Jellyfin's `From` prefix stands for Rustyfin's `To`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PathMapping {
    pub from: String,
    pub to: String,
}

/// Translates a Jellyfin path by the pair with the longest `From` that is a component-wise prefix
/// of `path` (trailing slashes ignored); `None` when no pair applies.
pub fn translate_path(path: &str, pairs: &[PathMapping]) -> Option<String> {
    let path = path.trim_end_matches('/');
    let mut best: Option<(&PathMapping, usize, &str)> = None;
    for pair in pairs.iter().filter(|pair| !pair.from.is_empty()) {
        let from = pair.from.trim_end_matches('/');
        let Some(rest) = strip_components(path, from) else {
            continue;
        };
        if best.is_none_or(|(_, longest, _)| from.len() > longest) {
            best = Some((pair, from.len(), rest));
        }
    }
    let (pair, _, rest) = best?;
    let translated = format!("{}{rest}", pair.to.trim_end_matches('/'));
    Some(if translated.is_empty() {
        "/".to_string()
    } else {
        translated
    })
}

/// The rest of `path` below `from`, when `from` is a whole-component prefix of it.
fn strip_components<'a>(path: &'a str, from: &str) -> Option<&'a str> {
    let rest = path.strip_prefix(from)?;
    (rest.is_empty() || rest.starts_with('/')).then_some(rest)
}

/// The suggested pairs (spec 2.8) and the Jellyfin library paths that none of them pairs with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestions {
    pub pairs: Vec<PathMapping>,
    pub unpaired: Vec<String>,
}

/// Suggests the pairs for `folders` (Jellyfin's `root/default/<Library>/`, sorted by name), then
/// `%MetadataPath%` and `%AppDataPath%`.
pub fn suggest(
    folders: &[LibraryFolder],
    libraries: &[Library],
    program_data: &Path,
    data_path: &Path,
) -> Suggestions {
    let mut pairs = Vec::new();
    let mut unpaired = Vec::new();
    for folder in folders {
        let library = choose_library(folder, libraries);
        for link in &folder.mblinks {
            match library.and_then(|library| pick_path(library, link)) {
                Some(to) => pairs.push(PathMapping {
                    from: link.clone(),
                    to,
                }),
                None => unpaired.push(link.clone()),
            }
        }
    }
    pairs.push(PathMapping {
        from: "%MetadataPath%".to_string(),
        to: program_data.join("metadata").display().to_string(),
    });
    pairs.push(PathMapping {
        from: "%AppDataPath%".to_string(),
        to: data_path.display().to_string(),
    });
    Suggestions { pairs, unpaired }
}

/// The Rustyfin library a Jellyfin library folder pairs with: the one of the same name (ambiguous
/// names give none), else the only one whose content type is the folder's `<type>.collection`.
fn choose_library<'a>(folder: &LibraryFolder, libraries: &'a [Library]) -> Option<&'a Library> {
    let wanted = folder.name.to_lowercase();
    let same_name: Vec<&Library> = libraries
        .iter()
        .filter(|library| library.name.to_lowercase() == wanted)
        .collect();
    match same_name.as_slice() {
        [only] => return Some(*only),
        [_, _, ..] => return None,
        [] => {}
    }
    let collection = folder.collection_type.as_deref()?;
    let typed: Vec<&Library> = libraries
        .iter()
        .filter(|library| library.content_type.jellyfin_collection_type() == collection)
        .collect();
    match typed.as_slice() {
        [only] => Some(*only),
        _ => None,
    }
}

/// The path of `library` that a Jellyfin media path pairs with: the one whose last component
/// matches (case-insensitively), else the library's only path.
fn pick_path(library: &Library, link: &str) -> Option<String> {
    let last = last_component(link).to_lowercase();
    let same_last: Vec<&String> = library
        .paths
        .iter()
        .filter(|path| last_component(path).to_lowercase() == last)
        .collect();
    if let [only] = same_last.as_slice() {
        return Some((*only).clone());
    }
    match library.paths.as_slice() {
        [only] => Some(only.clone()),
        _ => None,
    }
}

/// The last component of a path, ignoring trailing slashes.
fn last_component(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
}
