//! Import from Jellyfin (B20): users, watch data and track choices from a Jellyfin data folder,
//! read read-only. Spec: the B20 brief and spec, Stage 2.

pub mod job;
pub mod paths;
pub mod plan;
pub mod source;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::path::Path;

use chrono::Utc;
pub use job::{ImportJob, ImportStage, ImportStatus, JobHandle, JobState};
use paths::PathMapping;
use rustyfin_auth::{generate_setup_code, hash_setup_code, unusable_password_hash};
use rustyfin_model::user::User;
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
    /// Hashing a password or a setup code failed (a 500; the text names no secret).
    #[error("hashing a password or code failed")]
    Hash,
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
    let prepared = prepare(req).await?;
    let report = plan_report(db, req, &prepared, None).await?;

    let layout = report.layout;
    let source = &req.source_path;
    let user_count = report.users.len();
    let total = report.items.total;
    let (by_path, by_id, unmatched) = (
        report.items.matched_by_path,
        report.items.matched_by_id,
        report.items.unmatched,
    );
    tracing::info!(
        "jellyfin import preview: source {source}, layout {layout:?}, {user_count} users, {total} items ({by_path} by path, {by_id} by id, {unmatched} unmatched)"
    );
    Ok(report)
}

/// A source read for a run (spec 2.26).
pub struct Prepared {
    pub location: source::SourceLocation,
    pub snapshot: source::Snapshot,
    pub folders: Vec<source::LibraryFolder>,
}

/// Locates and reads the source of `req`, with the preview's errors (spec 2.17, 2.18, 2.26).
pub async fn prepare(req: &ImportRequest) -> Result<Prepared, ImportError> {
    let location = source::locate(Path::new(&req.source_path))?;
    let snapshot = source::read_snapshot(&location, req.layout).await?;
    let folders = source::read_library_folders(&location.program_data_path)?;
    Ok(Prepared {
        location,
        snapshot,
        folders,
    })
}

/// The planning `preview` and `run` share (spec 2.20, 2.26). A run passes its handle: it then
/// reports the Reading, Users and Matching stages and creates the users it plans to create, so
/// the items are matched with the final actions.
async fn plan_report(
    db: &SqlitePool,
    req: &ImportRequest,
    prepared: &Prepared,
    handle: Option<&JobHandle>,
) -> Result<ImportReport, ImportError> {
    if let Some(handle) = handle {
        handle.stage(ImportStage::Reading, 0);
    }
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
        &prepared.folders,
        &libraries,
        &prepared.location.program_data_path,
        &prepared.location.data_path,
    );
    let path_mappings = req
        .path_mappings
        .clone()
        .unwrap_or_else(|| suggested.pairs.clone());
    let mut users = plan::plan_users(
        &prepared.snapshot.users,
        req.user_ids.as_deref(),
        &rustyfin_users,
    );
    if let Some(handle) = handle {
        create_users(db, &mut users, rustyfin_users, handle).await?;
    }
    let importing: HashSet<Uuid> = users
        .iter()
        .filter(|user| matches!(user.action, UserAction::Match | UserAction::Create))
        .map(|user| user.jellyfin_id)
        .collect();
    let watched: HashSet<Uuid> = prepared
        .snapshot
        .user_rows
        .iter()
        .filter(|(_, user)| importing.contains(user))
        .map(|(item, _)| *item)
        .collect();
    if let Some(handle) = handle {
        handle.stage(ImportStage::Matching, 0);
    }
    let items = plan::match_items(&prepared.snapshot.items, &targets, &path_mappings, &watched);

    Ok(ImportReport {
        layout: prepared.snapshot.layout,
        data_path: prepared.location.data_path.display().to_string(),
        path_mappings,
        suggested_path_mappings: suggested.pairs,
        unpaired_library_paths: suggested.unpaired,
        users,
        items,
    })
}

/// The Users stage of a run (spec 2.7, 2.26): each row to create is matched again against
/// Rustyfin's users and the users created earlier in this run. An unmatched row gets a new user
/// with an unusable password and a one-time code, which the report shows once.
async fn create_users(
    db: &SqlitePool,
    users: &mut [UserRow],
    mut known: Vec<(Uuid, String)>,
    handle: &JobHandle,
) -> Result<(), ImportError> {
    let to_create: Vec<usize> = users
        .iter()
        .enumerate()
        .filter(|(_, user)| user.action == UserAction::Create)
        .map(|(index, _)| index)
        .collect();
    handle.stage(ImportStage::Users, to_create.len());
    for (done, index) in to_create.into_iter().enumerate() {
        let row = &mut users[index];
        match plan::match_user_name(&row.name, &known) {
            plan::UserMatch::Matched(id) => {
                row.action = UserAction::Match;
                row.user_id = Some(id);
            }
            plan::UserMatch::Ambiguous => {
                row.action = UserAction::Skip;
                row.reason = Some(SkipReason::Ambiguous);
            }
            plan::UserMatch::NoMatch => {
                let user = User {
                    id: Uuid::new_v4(),
                    name: row.name.clone(),
                    password_hash: unusable_password_hash()
                        .await
                        .map_err(|_| ImportError::Hash)?,
                    is_administrator: false,
                    created_at: Utc::now(),
                    last_login_date: None,
                    configuration: serde_json::Value::Object(Default::default()),
                };
                let code = generate_setup_code();
                let code_hash = hash_setup_code(&code)
                    .await
                    .map_err(|_| ImportError::Hash)?;
                rustyfin_db::repo_setup_code::create_user_with_code(db, &user, &code_hash)
                    .await
                    .map_err(ImportError::Db)?;
                known.push((user.id, user.name.clone()));
                row.user_id = Some(user.id);
                row.code = Some(code);
            }
        }
        handle.progress(done + 1);
    }
    Ok(())
}

/// Runs a prepared import in the background (spec 2.18-2.21, 2.26): creates the planned users and
/// reports through `handle`. One INFO line on success; a WARN line and the error on failure.
pub async fn run(db: SqlitePool, req: ImportRequest, prepared: Prepared, handle: JobHandle) {
    match plan_report(&db, &req, &prepared, Some(&handle)).await {
        Ok(report) => {
            let layout = report.layout;
            let source = &req.source_path;
            let user_count = report.users.len();
            let created = report
                .users
                .iter()
                .filter(|user| user.code.is_some())
                .count();
            let total = report.items.total;
            let (by_path, by_id, unmatched) = (
                report.items.matched_by_path,
                report.items.matched_by_id,
                report.items.unmatched,
            );
            tracing::info!(
                "jellyfin import run: source {source}, layout {layout:?}, {user_count} users, {created} created, {total} items ({by_path} by path, {by_id} by id, {unmatched} unmatched)"
            );
            handle.complete(report);
        }
        Err(error) => {
            tracing::warn!("jellyfin import run failed: {error}");
            handle.fail(error.to_string());
        }
    }
}
