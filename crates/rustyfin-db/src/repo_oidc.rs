//! The OIDC settings: the one row of `oidc_settings` (the migration inserts it). The client secret
//! is stored as it is (spec §4).

use rustyfin_model::oidc::OidcConfig;
use sqlx::SqlitePool;

#[derive(sqlx::FromRow)]
struct SettingsRow {
    enabled: bool,
    issuer_url: String,
    client_id: String,
    client_secret: String,
    scopes: String,
    button_label: String,
    username_claim: String,
    group_claim: String,
    allowed_group: String,
    admin_group: String,
    public_url: String,
    allow_password_sign_in: bool,
}

/// The saved settings.
pub async fn get_settings(pool: &SqlitePool) -> Result<OidcConfig, sqlx::Error> {
    let row = sqlx::query_as::<_, SettingsRow>(
        "SELECT enabled, issuer_url, client_id, client_secret, scopes, button_label, \
         username_claim, group_claim, allowed_group, admin_group, public_url, \
         allow_password_sign_in FROM oidc_settings WHERE id = 1",
    )
    .fetch_one(pool)
    .await?;
    Ok(OidcConfig {
        enabled: row.enabled,
        issuer_url: row.issuer_url,
        client_id: row.client_id,
        client_secret: row.client_secret,
        scopes: row.scopes,
        button_label: row.button_label,
        username_claim: row.username_claim,
        group_claim: row.group_claim,
        allowed_group: row.allowed_group,
        admin_group: row.admin_group,
        public_url: row.public_url,
        allow_password_sign_in: row.allow_password_sign_in,
    })
}

/// Writes every setting.
pub async fn save_settings(pool: &SqlitePool, config: &OidcConfig) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE oidc_settings SET enabled = ?, issuer_url = ?, client_id = ?, client_secret = ?, \
         scopes = ?, button_label = ?, username_claim = ?, group_claim = ?, allowed_group = ?, \
         admin_group = ?, public_url = ?, allow_password_sign_in = ? WHERE id = 1",
    )
    .bind(i32::from(config.enabled))
    .bind(&config.issuer_url)
    .bind(&config.client_id)
    .bind(&config.client_secret)
    .bind(&config.scopes)
    .bind(&config.button_label)
    .bind(&config.username_claim)
    .bind(&config.group_claim)
    .bind(&config.allowed_group)
    .bind(&config.admin_group)
    .bind(&config.public_url)
    .bind(i32::from(config.allow_password_sign_in))
    .execute(pool)
    .await?;
    Ok(())
}
