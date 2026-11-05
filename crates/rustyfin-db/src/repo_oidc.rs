//! The OIDC settings: the one row of `oidc_settings` (the migration inserts it). The client secret
//! is stored as it is (spec §4).

use chrono::{DateTime, Utc};
use rustyfin_model::oidc::OidcConfig;
use rustyfin_model::user::User;
use sqlx::SqlitePool;
use uuid::Uuid;

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

/// The user a provider account is linked to, if any.
pub async fn find_link(
    pool: &SqlitePool,
    issuer: &str,
    subject: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT user_id FROM user_oidc_links WHERE issuer = ? AND subject = ?")
            .bind(issuer)
            .bind(subject)
            .fetch_optional(pool)
            .await?;
    row.map(|(id,)| Uuid::parse_str(&id).map_err(|e| sqlx::Error::Decode(Box::new(e))))
        .transpose()
}

/// Whether the user is linked to a provider account.
pub async fn user_has_link(pool: &SqlitePool, user_id: Uuid) -> Result<bool, sqlx::Error> {
    let linked: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM user_oidc_links WHERE user_id = ?)")
            .bind(user_id.to_string())
            .fetch_one(pool)
            .await?;
    Ok(linked)
}

/// Links a user to a provider account. The executor is a pool, or a transaction the caller holds.
pub async fn create_link<'e, E>(
    executor: E,
    user_id: Uuid,
    issuer: &str,
    subject: &str,
    username: &str,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let at = now.to_rfc3339();
    sqlx::query(
        "INSERT INTO user_oidc_links (user_id, issuer, subject, username, linked_at, last_sign_in_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(user_id.to_string())
    .bind(issuer)
    .bind(subject)
    .bind(username)
    .bind(&at)
    .bind(&at)
    .execute(executor)
    .await?;
    Ok(())
}

/// Records a sign-in through the link. The username changes only when the claim gave one.
pub async fn touch_link(
    pool: &SqlitePool,
    user_id: Uuid,
    username: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE user_oidc_links SET last_sign_in_at = ?, username = COALESCE(?, username) \
         WHERE user_id = ?",
    )
    .bind(now.to_rfc3339())
    .bind(username)
    .bind(user_id.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

/// Creates a user and links it to a provider account in one transaction, so a failed link leaves
/// no user behind.
pub async fn create_linked_user(
    pool: &SqlitePool,
    user: &User,
    issuer: &str,
    subject: &str,
    username: &str,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    crate::repo_user::create_user(&mut *tx, user).await?;
    create_link(&mut *tx, user.id, issuer, subject, username, now).await?;
    tx.commit().await
}
