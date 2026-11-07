use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use unicase::UniCase;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct User {
    pub id: Uuid,
    pub name: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub is_administrator: bool,
    pub created_at: DateTime<Utc>,
    pub last_login_date: Option<DateTime<Utc>>,
    #[serde(default)]
    pub configuration: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserDto {
    pub id: Uuid,
    pub name: String,
    pub is_administrator: bool,
    pub last_login_date: Option<DateTime<Utc>>,
    pub configuration: serde_json::Value,
}

impl From<User> for UserDto {
    fn from(u: User) -> Self {
        Self {
            id: u.id,
            name: u.name,
            is_administrator: u.is_administrator,
            last_login_date: u.last_login_date,
            configuration: u.configuration,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PublicUserDto {
    pub id: Uuid,
    pub name: String,
}

impl From<User> for PublicUserDto {
    fn from(u: User) -> Self {
        Self {
            id: u.id,
            name: u.name,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct CreateUserRequest {
    pub name: String,
    pub password: String,
    /// Optional; the new user is an administrator only when this is true.
    pub is_administrator: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ChangePasswordRequest {
    /// Left out by a user without a password yet, and by an admin changing someone else's (AC25).
    pub current_pw: Option<String>,
    pub new_pw: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UpdatePolicyRequest {
    pub is_administrator: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ForgotPasswordRequest {
    pub entered_username: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ForgotPasswordResult {
    pub action: String,
    pub pin_file: String,
    pub pin_expiration_date: String,
}

/// Request to redeem a reset PIN with no username (`/Users/ForgotPassword/Pin`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ForgotPasswordPinRequest {
    pub pin: String,
}

/// Request to validate a password reset PIN code
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ResetPasswordRequest {
    pub pin: String,
    pub username: String,
    pub temp_password: String,
}

/// Request to update a user's password after PIN validation
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UpdatePasswordRequest {
    pub temp_password: String,
    pub new_password: String,
}

/// User security information
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserSecurityInfo {
    pub user_id: Uuid,
    pub name: String,
    pub is_administrator: bool,
    pub authentication_provider_id: String,
    pub authentication_provider_description: String,
    pub password_reset_provider_id: String,
    pub password_reset_provider_description: String,
    pub supports_two_step: bool,
}

/// User preferences
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserPreferences {
    pub chart_missing_episodes: String,
    pub chart_played_episodes: bool,
    pub group_episodes_by_season: bool,
    pub group_by_collection: bool,
    pub hide_audio_playlists_in_music: bool,
    pub hide_culture_titles: bool,
    pub hide_episodes: bool,
    pub hide_empty_series: bool,
    pub hide_seasons: bool,
    pub limit_field: String,
    pub metadata_country_code: String,
    pub preferred_metadata_language: String,
}

/// User parental control periods
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserAccessSchedule {
    pub day_of_week: String,
    pub start_hour: u32,
    pub end_hour: u32,
}

/// Request to update user name
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UpdateUserNameRequest {
    pub name: String,
}

/// Request to set easy PIN password
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct EasyPasswordRequest {
    pub easy_password: String,
}

/// User avatar image info
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserImageInfo {
    pub user_id: Uuid,
    pub image_type: String,
    pub id: String,
}

/// The form a username is compared by (Unicode D146): NFD, full case folding, NFKC, then folding
/// and NFKC again. It is only for comparing, never stored or shown.
pub fn name_key(name: &str) -> String {
    let nfd: String = name.nfd().collect();
    let folded = UniCase::new(nfd.as_str()).to_folded_case();
    let nfkc: String = folded.as_str().nfkc().collect();
    let refolded = UniCase::new(nfkc.as_str()).to_folded_case();
    refolded.as_str().nfkc().collect()
}

/// Whether two usernames match under Unicode caseless matching (D146): any case, full case
/// folding (ß matches ss), compatibility forms and composed or decomposed accents all match.
/// Accents, spaces and digits still count.
pub fn names_match(a: &str, b: &str) -> bool {
    name_key(a) == name_key(b)
}
