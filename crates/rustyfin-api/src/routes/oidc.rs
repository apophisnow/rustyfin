//! The OIDC settings (spec §3): the admin's read and save, and the public info the sign-in page
//! reads before anyone signs in (AC28).

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use rustyfin_model::oidc::{OidcConfig, OidcInfo, OidcSettings, OidcSettingsUpdate};

use crate::error::ApiError;
use crate::oidc::{discover, settings_answer};
use crate::state::AppState;

pub fn public_routes() -> Router<AppState> {
    Router::new().route("/Auth/Oidc/Info", get(info))
}

pub fn admin_routes() -> Router<AppState> {
    Router::new().route(
        "/Auth/Oidc/Settings",
        get(read_settings).post(save_settings),
    )
}

async fn info(State(state): State<AppState>) -> Result<Json<OidcInfo>, ApiError> {
    let config = rustyfin_db::repo_oidc::get_settings(&state.db).await?;
    Ok(Json(OidcInfo {
        enabled: config.enabled,
        button_label: config.button_label,
    }))
}

async fn read_settings(State(state): State<AppState>) -> Result<Json<OidcSettings>, ApiError> {
    let config = rustyfin_db::repo_oidc::get_settings(&state.db).await?;
    Ok(Json(settings_answer(&config)))
}

/// Saves the sent fields over the saved ones. The body is read as a JSON value first and then
/// converted, so a serde message (which can echo a value such as a numeric secret) is replaced by
/// a fixed sentence and never reaches the answer.
async fn save_settings(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<OidcSettings>, ApiError> {
    let update: OidcSettingsUpdate = serde_json::from_value(body).map_err(|_| {
        ApiError::BadRequest("The OIDC settings are not in the expected form.".to_string())
    })?;
    let current = rustyfin_db::repo_oidc::get_settings(&state.db).await?;
    let next = update.apply(current);
    check_fields(&next).map_err(ApiError::BadRequest)?;
    if next.enabled {
        // Always a fresh fetch: the cache is never used to accept a provider.
        discover(&state.oidc.http, &next.issuer_url)
            .await
            .map_err(ApiError::BadRequest)?;
    }
    rustyfin_db::repo_oidc::save_settings(&state.db, &next).await?;
    state.oidc.forget().await;
    Ok(Json(settings_answer(&next)))
}

/// The checks a save must pass when OIDC is on (AC3). Nothing is checked while it is off.
fn check_fields(config: &OidcConfig) -> Result<(), String> {
    if !config.enabled {
        return Ok(());
    }
    check_url("The issuer URL", &config.issuer_url)?;
    check_url("The public URL", &config.public_url)?;
    if config.client_id.is_empty() {
        return Err("The client ID is required.".to_string());
    }
    if config.username_claim.is_empty() {
        return Err("The username claim is required.".to_string());
    }
    if config.group_set() && config.group_claim.is_empty() {
        return Err("The group claim is required when a group is set.".to_string());
    }
    Ok(())
}

/// An absolute http or https URL with no query and no fragment.
fn check_url(label: &str, value: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(value)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
        .ok_or_else(|| format!("{label} must be an absolute http or https URL."))?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(format!("{label} must not have a query or a fragment."));
    }
    Ok(())
}
