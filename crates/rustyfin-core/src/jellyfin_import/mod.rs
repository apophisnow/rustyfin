//! Import from Jellyfin (B20): users, watch data and track choices from a Jellyfin data folder,
//! read read-only. Spec: the B20 brief and spec, Stage 2.

pub mod paths;
pub mod plan;
pub mod source;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::path::Path;

use paths::PathMapping;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use uuid::Uuid;

/// Why an import can't read its source, or failed writing.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// The source folder or one of its databases is unusable (a 400; names the file, no row data).
    #[error("{0}")]
    BadSource(String),
    /// Reading or writing Rustyfin's own database failed (a 500). Deliberately no `From`, so a
    /// failed read of the source can't become a 500 by `?`; source errors are `BadSource`.
    #[error(transparent)]
    Db(sqlx::Error),
}

/// The layout a request asks for (spec 2.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum LayoutChoice {
    #[default]
    Auto,
    New,
    Old,
}

/// The layout a source was read as (spec 2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Layout {
    New,
    Old,
}

/// An import request (spec 2.17). Unknown fields, such as `MetadataMode`, are ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ImportRequest {
    pub source_path: String,
    #[serde(default)]
    pub layout: LayoutChoice,
    /// Absent: use the suggested pairs. Present (even empty): use exactly these.
    pub path_mappings: Option<Vec<PathMapping>>,
    /// Jellyfin user ids to import; absent means all users.
    pub user_ids: Option<Vec<Uuid>>,
}

/// The preview's answer (spec 2.20). `WatchData`, `Tracks` and `Metadata` come in later stages.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ImportReport {
    pub layout: Layout,
    pub data_path: String,
    pub path_mappings: Vec<PathMapping>,
    pub suggested_path_mappings: Vec<PathMapping>,
    pub unpaired_library_paths: Vec<String>,
    pub users: Vec<UserRow>,
    pub items: ItemReport,
}

/// One Jellyfin user and what the import does with them (spec 2.5, 2.6, 2.20).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserRow {
    pub jellyfin_id: Uuid,
    pub name: String,
    pub was_administrator: bool,
    pub action: UserAction,
    pub reason: Option<SkipReason>,
    /// The Rustyfin user: the match for `Match`; null for `Create` in a preview.
    pub user_id: Option<Uuid>,
    /// The one-time code of a created user; only a run has codes.
    pub code: Option<String>,
}

/// What an import does with a Jellyfin user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum UserAction {
    Match,
    Create,
    Skip,
}

/// Why a Jellyfin user is skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SkipReason {
    NotSelected,
    Ambiguous,
}

/// The item counts of the report (spec 2.11, 2.20). The three counts add up to `total`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ItemReport {
    pub total: usize,
    pub matched_by_path: usize,
    pub matched_by_id: usize,
    pub unmatched: usize,
    pub unmatched_samples: Vec<UnmatchedSample>,
}

/// An unmatched item that has watch data for an imported user.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct UnmatchedSample {
    #[serde(rename = "Type")]
    pub item_type: String,
    pub name: String,
    pub path: String,
}

/// Reads the Jellyfin data folder named by `req` and reports what an import would do. Writes
/// nothing, here or in the source.
pub async fn preview(db: &SqlitePool, req: &ImportRequest) -> Result<ImportReport, ImportError> {
    let location = source::locate(Path::new(&req.source_path))?;
    let snapshot = source::read_snapshot(&location, req.layout).await?;
    let folders = source::read_library_folders(&location.program_data_path)?;
    let libraries = rustyfin_db::repo_library::get_all_libraries(db)
        .await
        .map_err(ImportError::Db)?;
    let rustyfin_users = rustyfin_db::repo_jellyfin_import::user_names(db)
        .await
        .map_err(ImportError::Db)?;
    let targets = rustyfin_db::repo_jellyfin_import::import_targets(db)
        .await
        .map_err(ImportError::Db)?;

    let suggested = paths::suggest(
        &folders,
        &libraries,
        &location.program_data_path,
        &location.data_path,
    );
    let path_mappings = req
        .path_mappings
        .clone()
        .unwrap_or_else(|| suggested.pairs.clone());
    let users = plan::plan_users(&snapshot.users, req.user_ids.as_deref(), &rustyfin_users);
    let importing: HashSet<Uuid> = users
        .iter()
        .filter(|user| matches!(user.action, UserAction::Match | UserAction::Create))
        .map(|user| user.jellyfin_id)
        .collect();
    let watched: HashSet<Uuid> = snapshot
        .user_rows
        .iter()
        .filter(|(_, user)| importing.contains(user))
        .map(|(item, _)| *item)
        .collect();
    let items = plan::match_items(&snapshot.items, &targets, &path_mappings, &watched);

    let layout = snapshot.layout;
    let source = &req.source_path;
    let user_count = users.len();
    let total = items.total;
    let (by_path, by_id, unmatched) = (items.matched_by_path, items.matched_by_id, items.unmatched);
    tracing::info!(
        "jellyfin import preview: source {source}, layout {layout:?}, {user_count} users, {total} items ({by_path} by path, {by_id} by id, {unmatched} unmatched)"
    );

    Ok(ImportReport {
        layout: snapshot.layout,
        data_path: location.data_path.display().to_string(),
        path_mappings,
        suggested_path_mappings: suggested.pairs,
        unpaired_library_paths: suggested.unpaired,
        users,
        items,
    })
}
