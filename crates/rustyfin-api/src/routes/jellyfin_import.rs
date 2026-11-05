//! The Jellyfin import preview (B20 stage 2a, spec 2.17): admin only, writes nothing.

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use rustyfin_core::jellyfin_import::{ImportError, ImportReport, ImportRequest, preview};

use crate::error::ApiError;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/JellyfinImport/Preview", post(preview_route))
}

async fn preview_route(
    State(state): State<AppState>,
    Json(req): Json<ImportRequest>,
) -> Result<Json<ImportReport>, ApiError> {
    let report = preview(&state.db_ro, &req).await.map_err(|e| match e {
        ImportError::BadSource(msg) => ApiError::BadRequest(msg),
        ImportError::Db(e) => ApiError::from(e),
    })?;
    Ok(Json(report))
}
