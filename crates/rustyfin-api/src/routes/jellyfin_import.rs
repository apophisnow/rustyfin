//! The Jellyfin import (B20 stages 2a and 2b, spec 2.17-2.19, 2.27): admin only. Preview writes
//! nothing; a run goes through the server's one import job, and its status is read from there.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use rustyfin_core::jellyfin_import::{
    ImportError, ImportReport, ImportRequest, ImportStatus, prepare, preview, run,
};

use crate::error::ApiError;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/JellyfinImport/Preview", post(preview_route))
        .route("/JellyfinImport/Run", post(run_route))
        .route("/JellyfinImport/Status", get(status_route))
}

/// A bad source is a 400 naming the file; a failed write or hash is a 500.
fn import_error(error: ImportError) -> ApiError {
    match error {
        ImportError::BadSource(msg) => ApiError::BadRequest(msg),
        ImportError::Db(e) => ApiError::from(e),
        ImportError::Hash => ApiError::Internal("An internal error occurred.".to_string()),
    }
}

async fn preview_route(
    State(state): State<AppState>,
    Json(req): Json<ImportRequest>,
) -> Result<Json<ImportReport>, ApiError> {
    let report = preview(&state.db_ro, &req).await.map_err(import_error)?;
    Ok(Json(report))
}

/// Starts the import in a spawned task: 202 `{"State": "Running"}`, or 409 while one runs. The
/// source is read first, so a bad source is refused before any run starts.
async fn run_route(
    State(state): State<AppState>,
    Json(req): Json<ImportRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let prepared = prepare(&req).await.map_err(import_error)?;
    let handle = state
        .jellyfin_import
        .try_start()
        .ok_or_else(|| ApiError::Conflict("An import is already running.".to_string()))?;
    tokio::spawn(run(state.db.clone(), req, prepared, handle));
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "State": "Running" })),
    ))
}

async fn status_route(State(state): State<AppState>) -> Json<ImportStatus> {
    Json(state.jellyfin_import.status())
}
