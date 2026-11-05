//! One-time setup codes of users the Jellyfin import creates (B20 spec 2.22, 2.24, 3.1). Only an
//! argon2 hash of a code is stored.

use rustyfin_model::user::User;
use sqlx::SqlitePool;

/// Inserts `user` (the columns `repo_user::create_user` writes) and its code row in one
/// transaction; the code row's `created_at` is the user's, as RFC 3339.
pub async fn create_user_with_code(
    pool: &SqlitePool,
    user: &User,
    code_hash: &str,
) -> Result<(), sqlx::Error> {
    let id = user.id.to_string();
    let created = user.created_at.to_rfc3339();
    let is_admin = user.is_administrator as i32;
    let config = user.configuration.to_string();

    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO users (id, name, password_hash, is_administrator, created_at, configuration) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&user.name)
    .bind(&user.password_hash)
    .bind(is_admin)
    .bind(&created)
    .bind(&config)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO user_setup_codes (user_id, code_hash, created_at) VALUES (?, ?, ?)")
        .bind(&id)
        .bind(code_hash)
        .bind(&created)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}
