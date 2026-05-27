//! Alert notification system database models
//!
//! Implements pub/sub model for alerts: channels, destinations, subscriptions

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::error::Error;
use std::fmt;
use uuid::Uuid;

// =============================================================================
// Regex Cache for Channel Pattern Matching
// =============================================================================

/// Cache of compiled regex patterns to avoid recompilation on every alert publish.
///
/// Uses parking_lot::Mutex (no poisoning) because this cache is shared across
/// alert-publish requests; a panic during one request must not permanently disable
/// regex compilation for all subsequent ones.
static REGEX_CACHE: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<String, regex::Regex>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// Get a compiled regex from cache, or compile and cache it
fn get_or_compile_regex(pattern: &str) -> Result<regex::Regex, regex::Error> {
    let mut cache = REGEX_CACHE.lock();
    if let Some(re) = cache.get(pattern) {
        return Ok(re.clone());
    }
    let re = regex::Regex::new(pattern)?;
    cache.insert(pattern.to_string(), re.clone());
    Ok(re)
}

// =============================================================================
// Error Types
// =============================================================================

#[derive(Debug)]
pub enum AlertError {
    Database(sqlx::Error),
    NotFound,
    InvalidPattern(String),
    Unauthorized,
    Other(String),
}

impl fmt::Display for AlertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AlertError::Database(e) => write!(f, "Database error: {}", e),
            AlertError::NotFound => write!(f, "Alert resource not found"),
            AlertError::InvalidPattern(msg) => write!(f, "Invalid regex pattern: {}", msg),
            AlertError::Unauthorized => write!(f, "Unauthorized access"),
            AlertError::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl Error for AlertError {}

impl From<sqlx::Error> for AlertError {
    fn from(error: sqlx::Error) -> Self {
        AlertError::Database(error)
    }
}

// =============================================================================
// Destination Type
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(i16)]
pub enum DestinationType {
    Email = 1,
    Slack = 2,
    PagerDuty = 3,
    Webhook = 4,
}

impl DestinationType {
    pub fn from_i16(value: i16) -> Option<Self> {
        match value {
            1 => Some(DestinationType::Email),
            2 => Some(DestinationType::Slack),
            3 => Some(DestinationType::PagerDuty),
            4 => Some(DestinationType::Webhook),
            _ => None,
        }
    }

    pub fn as_i16(&self) -> i16 {
        *self as i16
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            DestinationType::Email => "email",
            DestinationType::Slack => "slack",
            DestinationType::PagerDuty => "pagerduty",
            DestinationType::Webhook => "webhook",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "email" => Some(DestinationType::Email),
            "slack" => Some(DestinationType::Slack),
            "pagerduty" => Some(DestinationType::PagerDuty),
            "webhook" => Some(DestinationType::Webhook),
            _ => None,
        }
    }
}

// =============================================================================
// Delivery Status
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(i16)]
pub enum DeliveryStatus {
    Pending = 1,
    Sent = 2,
    Failed = 3,
    Retrying = 4,
}

impl DeliveryStatus {
    pub fn from_i16(value: i16) -> Option<Self> {
        match value {
            1 => Some(DeliveryStatus::Pending),
            2 => Some(DeliveryStatus::Sent),
            3 => Some(DeliveryStatus::Failed),
            4 => Some(DeliveryStatus::Retrying),
            _ => None,
        }
    }

    pub fn as_i16(&self) -> i16 {
        *self as i16
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            DeliveryStatus::Pending => "pending",
            DeliveryStatus::Sent => "sent",
            DeliveryStatus::Failed => "failed",
            DeliveryStatus::Retrying => "retrying",
        }
    }
}

// =============================================================================
// Alert Destination
// =============================================================================

/// Named delivery endpoint (email, Slack, PagerDuty, webhook)
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct AlertDestination {
    pub alert_destination_id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub destination_type_id: i16,
    pub config: serde_json::Value,
    pub enabled: bool,
    pub verified: bool,
    pub verification_token: Option<String>,
    pub verification_expires_at: Option<DateTime<Utc>>,
    pub verification_attempts: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by_user_id: Uuid,
    pub updated_by_user_id: Option<Uuid>,
}

impl AlertDestination {
    /// Get destination type enum
    pub fn destination_type(&self) -> Option<DestinationType> {
        DestinationType::from_i16(self.destination_type_id)
    }

    /// Get all destinations for an org
    pub async fn get_by_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
    ) -> Result<Vec<AlertDestination>, AlertError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                let destinations = sqlx::query_as::<_, AlertDestination>(
                    "SELECT * FROM alert_destination WHERE org_id = $1 ORDER BY name",
                )
                .bind(org_id)
                .fetch_all(pool)
                .await?;
                Ok(destinations)
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                let destinations = sqlx::query_as::<_, AlertDestination>(
                    "SELECT * FROM alert_destination WHERE org_id = ? ORDER BY name",
                )
                .bind(org_id)
                .fetch_all(pool)
                .await?;
                Ok(destinations)
            }
        }
    }

    /// Get destination by ID
    pub async fn get_by_id(
        db: &crate::db::DatabasePool,
        destination_id: &Uuid,
    ) -> Result<AlertDestination, AlertError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => sqlx::query_as::<_, AlertDestination>(
                "SELECT * FROM alert_destination WHERE alert_destination_id = $1",
            )
            .bind(destination_id)
            .fetch_optional(pool)
            .await?
            .ok_or(AlertError::NotFound),
            crate::db::DatabasePool::Sqlite(pool) => sqlx::query_as::<_, AlertDestination>(
                "SELECT * FROM alert_destination WHERE alert_destination_id = ?",
            )
            .bind(destination_id)
            .fetch_optional(pool)
            .await?
            .ok_or(AlertError::NotFound),
        }
    }

    /// Create a new destination
    pub async fn create(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        name: &str,
        destination_type: DestinationType,
        config: &serde_json::Value,
        created_by_user_id: &Uuid,
    ) -> Result<AlertDestination, AlertError> {
        Self::create_with_verification(
            db,
            org_id,
            name,
            destination_type,
            config,
            created_by_user_id,
            true,
            None,
            None,
        )
        .await
    }

    /// Create a new destination with verification state
    #[allow(clippy::too_many_arguments)]
    pub async fn create_with_verification(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        name: &str,
        destination_type: DestinationType,
        config: &serde_json::Value,
        created_by_user_id: &Uuid,
        verified: bool,
        verification_token: Option<&str>,
        verification_expires_at: Option<DateTime<Utc>>,
    ) -> Result<AlertDestination, AlertError> {
        let destination_id = Uuid::now_v7();
        let now = Utc::now();

        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO alert_destination (alert_destination_id, org_id, name, destination_type_id, config, verified, verification_token, verification_expires_at, created_by_user_id, created_at, updated_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $10)",
                )
                .bind(destination_id)
                .bind(org_id)
                .bind(name)
                .bind(destination_type.as_i16())
                .bind(config)
                .bind(verified)
                .bind(verification_token)
                .bind(verification_expires_at)
                .bind(created_by_user_id)
                .bind(now)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "INSERT INTO alert_destination (alert_destination_id, org_id, name, destination_type_id, config, verified, verification_token, verification_expires_at, created_by_user_id, created_at, updated_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(destination_id)
                .bind(org_id)
                .bind(name)
                .bind(destination_type.as_i16())
                .bind(config.to_string())
                .bind(verified)
                .bind(verification_token)
                .bind(verification_expires_at)
                .bind(created_by_user_id)
                .bind(now)
                .bind(now)
                .execute(pool)
                .await?;
            }
        }

        Self::get_by_id(db, &destination_id).await
    }

    /// Update a destination
    pub async fn update(
        db: &crate::db::DatabasePool,
        destination_id: &Uuid,
        name: &str,
        config: &serde_json::Value,
        enabled: bool,
        updated_by_user_id: &Uuid,
    ) -> Result<AlertDestination, AlertError> {
        let now = Utc::now();

        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "UPDATE alert_destination SET name = $1, config = $2, enabled = $3, updated_at = $4, updated_by_user_id = $5 WHERE alert_destination_id = $6",
                )
                .bind(name)
                .bind(config)
                .bind(enabled)
                .bind(now)
                .bind(updated_by_user_id)
                .bind(destination_id)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE alert_destination SET name = ?, config = ?, enabled = ?, updated_at = ?, updated_by_user_id = ? WHERE alert_destination_id = ?",
                )
                .bind(name)
                .bind(config.to_string())
                .bind(enabled)
                .bind(now)
                .bind(updated_by_user_id)
                .bind(destination_id)
                .execute(pool)
                .await?;
            }
        }

        Self::get_by_id(db, destination_id).await
    }

    /// Delete a destination.
    ///
    /// Removes subscription-destination links first (the join table FK is
    /// `ON DELETE RESTRICT`), then deletes the destination itself.
    pub async fn delete(
        db: &crate::db::DatabasePool,
        destination_id: &Uuid,
    ) -> Result<(), AlertError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                // Remove join-table links that reference this destination
                sqlx::query("DELETE FROM alert_subscription_destination WHERE destination_id = $1")
                    .bind(destination_id)
                    .execute(pool)
                    .await?;

                sqlx::query("DELETE FROM alert_destination WHERE alert_destination_id = $1")
                    .bind(destination_id)
                    .execute(pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                // Remove join-table links that reference this destination
                sqlx::query("DELETE FROM alert_subscription_destination WHERE destination_id = ?")
                    .bind(destination_id)
                    .execute(pool)
                    .await?;

                sqlx::query("DELETE FROM alert_destination WHERE alert_destination_id = ?")
                    .bind(destination_id)
                    .execute(pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Look up a destination by its verification token and mark it as verified.
    /// Returns the destination if found and valid, or an error otherwise.
    pub async fn verify_by_token(
        db: &crate::db::DatabasePool,
        token: &str,
    ) -> Result<AlertDestination, AlertError> {
        // Find destination by token
        let dest = match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query_as::<_, AlertDestination>(
                    "SELECT * FROM alert_destination WHERE verification_token = $1",
                )
                .bind(token)
                .fetch_optional(pool)
                .await?
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query_as::<_, AlertDestination>(
                    "SELECT * FROM alert_destination WHERE verification_token = ?",
                )
                .bind(token)
                .fetch_optional(pool)
                .await?
            }
        }
        .ok_or(AlertError::NotFound)?;

        // Already verified
        if dest.verified {
            return Ok(dest);
        }

        // Check expiration
        if let Some(expires_at) = dest.verification_expires_at
            && expires_at < Utc::now()
        {
            return Err(AlertError::Other(
                "Verification link has expired. Please request a new one.".to_string(),
            ));
        }

        // Mark as verified, clear token
        let now = Utc::now();
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "UPDATE alert_destination SET verified = true, verification_token = NULL, verification_expires_at = NULL, updated_at = $1 WHERE alert_destination_id = $2",
                )
                .bind(now)
                .bind(dest.alert_destination_id)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE alert_destination SET verified = 1, verification_token = NULL, verification_expires_at = NULL, updated_at = ? WHERE alert_destination_id = ?",
                )
                .bind(now)
                .bind(dest.alert_destination_id)
                .execute(pool)
                .await?;
            }
        }

        Self::get_by_id(db, &dest.alert_destination_id).await
    }

    /// Update the verification token for resending a verification email.
    /// Also refreshes the expiration and increments the attempt counter.
    pub async fn refresh_verification_token(
        db: &crate::db::DatabasePool,
        destination_id: &Uuid,
        new_token: &str,
        new_expires_at: DateTime<Utc>,
    ) -> Result<(), AlertError> {
        let now = Utc::now();
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "UPDATE alert_destination SET verification_token = $1, verification_expires_at = $2, verification_attempts = verification_attempts + 1, updated_at = $3 WHERE alert_destination_id = $4",
                )
                .bind(new_token)
                .bind(new_expires_at)
                .bind(now)
                .bind(destination_id)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE alert_destination SET verification_token = ?, verification_expires_at = ?, verification_attempts = verification_attempts + 1, updated_at = ? WHERE alert_destination_id = ?",
                )
                .bind(new_token)
                .bind(new_expires_at)
                .bind(now)
                .bind(destination_id)
                .execute(pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Reset verification state (e.g. when the email address is changed).
    /// Marks the destination as unverified, sets a new token, resets attempts.
    pub async fn reset_verification(
        db: &crate::db::DatabasePool,
        destination_id: &Uuid,
        new_token: &str,
        new_expires_at: DateTime<Utc>,
    ) -> Result<(), AlertError> {
        let now = Utc::now();
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "UPDATE alert_destination SET verified = false, verification_token = $1, verification_expires_at = $2, verification_attempts = 0, updated_at = $3 WHERE alert_destination_id = $4",
                )
                .bind(new_token)
                .bind(new_expires_at)
                .bind(now)
                .bind(destination_id)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE alert_destination SET verified = 0, verification_token = ?, verification_expires_at = ?, verification_attempts = 0, updated_at = ? WHERE alert_destination_id = ?",
                )
                .bind(new_token)
                .bind(new_expires_at)
                .bind(now)
                .bind(destination_id)
                .execute(pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Generate a secure random verification token (64-char alphanumeric)
    pub fn generate_verification_token() -> String {
        crate::db::email_verification::EmailVerification::generate_token()
    }
}

// =============================================================================
// Alert Channel
// =============================================================================

/// Named channel definition with regex pattern
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct AlertChannel {
    pub alert_channel_id: Uuid,
    pub org_id: Option<Uuid>,
    pub env_id: Option<Uuid>,
    pub name: String,
    pub pattern: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by_user_id: Option<Uuid>,
    pub updated_by_user_id: Option<Uuid>,
}

impl AlertChannel {
    /// Check if this is a system-wide channel (built-in, read-only)
    pub fn is_system(&self) -> bool {
        self.org_id.is_none()
    }

    /// Get all channels for an org (including system channels)
    pub async fn get_by_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
    ) -> Result<Vec<AlertChannel>, AlertError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                let channels = sqlx::query_as::<_, AlertChannel>(
                    "SELECT * FROM alert_channel WHERE org_id IS NULL OR org_id = $1 ORDER BY org_id NULLS FIRST, name",
                )
                .bind(org_id)
                .fetch_all(pool)
                .await?;
                Ok(channels)
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                let channels = sqlx::query_as::<_, AlertChannel>(
                    "SELECT * FROM alert_channel WHERE org_id IS NULL OR org_id = ? ORDER BY org_id IS NULL DESC, name",
                )
                .bind(org_id)
                .fetch_all(pool)
                .await?;
                Ok(channels)
            }
        }
    }

    /// Get system-wide channels only
    pub async fn get_system_channels(
        db: &crate::db::DatabasePool,
    ) -> Result<Vec<AlertChannel>, AlertError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                let channels = sqlx::query_as::<_, AlertChannel>(
                    "SELECT * FROM alert_channel WHERE org_id IS NULL ORDER BY name",
                )
                .fetch_all(pool)
                .await?;
                Ok(channels)
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                let channels = sqlx::query_as::<_, AlertChannel>(
                    "SELECT * FROM alert_channel WHERE org_id IS NULL ORDER BY name",
                )
                .fetch_all(pool)
                .await?;
                Ok(channels)
            }
        }
    }

    /// Get channel by ID
    pub async fn get_by_id(
        db: &crate::db::DatabasePool,
        channel_id: &Uuid,
    ) -> Result<AlertChannel, AlertError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => sqlx::query_as::<_, AlertChannel>(
                "SELECT * FROM alert_channel WHERE alert_channel_id = $1",
            )
            .bind(channel_id)
            .fetch_optional(pool)
            .await?
            .ok_or(AlertError::NotFound),
            crate::db::DatabasePool::Sqlite(pool) => sqlx::query_as::<_, AlertChannel>(
                "SELECT * FROM alert_channel WHERE alert_channel_id = ?",
            )
            .bind(channel_id)
            .fetch_optional(pool)
            .await?
            .ok_or(AlertError::NotFound),
        }
    }

    /// Validate a regex pattern
    pub fn validate_pattern(pattern: &str) -> Result<(), AlertError> {
        regex::Regex::new(pattern)
            .map(|_| ())
            .map_err(|e| AlertError::InvalidPattern(e.to_string()))
    }

    /// Create a new custom channel
    pub async fn create(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        env_id: Option<&Uuid>,
        name: &str,
        pattern: &str,
        created_by_user_id: &Uuid,
    ) -> Result<AlertChannel, AlertError> {
        // Validate pattern first
        Self::validate_pattern(pattern)?;

        let channel_id = Uuid::now_v7();
        let now = Utc::now();

        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO alert_channel (alert_channel_id, org_id, env_id, name, pattern, created_by_user_id, created_at, updated_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $7)",
                )
                .bind(channel_id)
                .bind(org_id)
                .bind(env_id)
                .bind(name)
                .bind(pattern)
                .bind(created_by_user_id)
                .bind(now)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "INSERT INTO alert_channel (alert_channel_id, org_id, env_id, name, pattern, created_by_user_id, created_at, updated_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(channel_id)
                .bind(org_id)
                .bind(env_id)
                .bind(name)
                .bind(pattern)
                .bind(created_by_user_id)
                .bind(now)
                .bind(now)
                .execute(pool)
                .await?;
            }
        }

        Self::get_by_id(db, &channel_id).await
    }

    /// Update a custom channel
    pub async fn update(
        db: &crate::db::DatabasePool,
        channel_id: &Uuid,
        name: &str,
        pattern: &str,
        env_id: Option<&Uuid>,
        enabled: bool,
        updated_by_user_id: &Uuid,
    ) -> Result<AlertChannel, AlertError> {
        // Validate pattern first
        Self::validate_pattern(pattern)?;

        let now = Utc::now();

        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                let result = sqlx::query(
                    "UPDATE alert_channel
                     SET name = $1, pattern = $2, env_id = $3, enabled = $4, updated_by_user_id = $5, updated_at = $6
                     WHERE alert_channel_id = $7 AND org_id IS NOT NULL",
                )
                .bind(name)
                .bind(pattern)
                .bind(env_id)
                .bind(enabled)
                .bind(updated_by_user_id)
                .bind(now)
                .bind(channel_id)
                .execute(pool)
                .await?;

                if result.rows_affected() == 0 {
                    return Err(AlertError::Unauthorized);
                }
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                let result = sqlx::query(
                    "UPDATE alert_channel
                     SET name = ?, pattern = ?, env_id = ?, enabled = ?, updated_by_user_id = ?, updated_at = ?
                     WHERE alert_channel_id = ? AND org_id IS NOT NULL",
                )
                .bind(name)
                .bind(pattern)
                .bind(env_id)
                .bind(enabled)
                .bind(updated_by_user_id)
                .bind(now)
                .bind(channel_id)
                .execute(pool)
                .await?;

                if result.rows_affected() == 0 {
                    return Err(AlertError::Unauthorized);
                }
            }
        }

        Self::get_by_id(db, channel_id).await
    }

    /// Delete a custom channel (cannot delete system channels)
    pub async fn delete(db: &crate::db::DatabasePool, channel_id: &Uuid) -> Result<(), AlertError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "DELETE FROM alert_channel WHERE alert_channel_id = $1 AND org_id IS NOT NULL",
                )
                .bind(channel_id)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "DELETE FROM alert_channel WHERE alert_channel_id = ? AND org_id IS NOT NULL",
                )
                .bind(channel_id)
                .execute(pool)
                .await?;
            }
        }
        Ok(())
    }
}

// =============================================================================
// Alert Subscription
// =============================================================================

/// Subscription linking subscriber (org/team/user) to channels and destinations
///
/// Note: `team_id` and `user_id` fields exist in the schema for future team-level
/// and user-level subscriptions, but are currently unused. All subscriptions are
/// org-level (team_id = NULL, user_id = NULL). The DB constraint `chk_subscriber_type`
/// ensures team_id and user_id are mutually exclusive.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct AlertSubscription {
    pub alert_subscription_id: Uuid,
    pub org_id: Uuid,
    pub env_id: Option<Uuid>,
    pub team_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by_user_id: Uuid,
    pub updated_by_user_id: Option<Uuid>,
}

impl AlertSubscription {
    /// Get subscriber type label
    pub fn subscriber_type(&self) -> &'static str {
        if self.user_id.is_some() {
            "user"
        } else if self.team_id.is_some() {
            "team"
        } else {
            "org"
        }
    }

    /// Get all subscriptions for an org
    pub async fn get_by_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
    ) -> Result<Vec<AlertSubscription>, AlertError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                let subscriptions = sqlx::query_as::<_, AlertSubscription>(
                    "SELECT * FROM alert_subscription WHERE org_id = $1 ORDER BY created_at DESC",
                )
                .bind(org_id)
                .fetch_all(pool)
                .await?;
                Ok(subscriptions)
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                let subscriptions = sqlx::query_as::<_, AlertSubscription>(
                    "SELECT * FROM alert_subscription WHERE org_id = ? ORDER BY created_at DESC",
                )
                .bind(org_id)
                .fetch_all(pool)
                .await?;
                Ok(subscriptions)
            }
        }
    }

    /// Get subscription by ID
    pub async fn get_by_id(
        db: &crate::db::DatabasePool,
        alert_subscription_id: &Uuid,
    ) -> Result<AlertSubscription, AlertError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => sqlx::query_as::<_, AlertSubscription>(
                "SELECT * FROM alert_subscription WHERE alert_subscription_id = $1",
            )
            .bind(alert_subscription_id)
            .fetch_optional(pool)
            .await?
            .ok_or(AlertError::NotFound),
            crate::db::DatabasePool::Sqlite(pool) => sqlx::query_as::<_, AlertSubscription>(
                "SELECT * FROM alert_subscription WHERE alert_subscription_id = ?",
            )
            .bind(alert_subscription_id)
            .fetch_optional(pool)
            .await?
            .ok_or(AlertError::NotFound),
        }
    }

    /// Create a new subscription with channels and destinations
    pub async fn create(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        env_id: Option<&Uuid>,
        channel_ids: &[Uuid],
        destination_ids: &[Uuid],
        created_by_user_id: &Uuid,
    ) -> Result<AlertSubscription, AlertError> {
        let alert_subscription_id = Uuid::now_v7();
        let now = Utc::now();

        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                // Start transaction
                let mut tx = pool.begin().await?;

                // Create subscription
                sqlx::query(
                    "INSERT INTO alert_subscription (alert_subscription_id, org_id, env_id, enabled, created_at, updated_at, created_by_user_id)
                     VALUES ($1, $2, $3, true, $4, $4, $5)",
                )
                .bind(alert_subscription_id)
                .bind(org_id)
                .bind(env_id)
                .bind(now)
                .bind(created_by_user_id)
                .execute(&mut *tx)
                .await?;

                // Link channels
                for channel_id in channel_ids {
                    sqlx::query(
                        "INSERT INTO alert_subscription_channel (subscription_id, channel_id, enabled, created_at)
                         VALUES ($1, $2, true, $3)",
                    )
                    .bind(alert_subscription_id)
                    .bind(channel_id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                }

                // Link destinations
                for destination_id in destination_ids {
                    sqlx::query(
                        "INSERT INTO alert_subscription_destination (subscription_id, destination_id, enabled, created_at)
                         VALUES ($1, $2, true, $3)",
                    )
                    .bind(alert_subscription_id)
                    .bind(destination_id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                }

                tx.commit().await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                // Start transaction
                let mut tx = pool.begin().await?;

                // Create subscription
                sqlx::query(
                    "INSERT INTO alert_subscription (alert_subscription_id, org_id, env_id, enabled, created_at, updated_at, created_by_user_id)
                     VALUES (?, ?, ?, 1, ?, ?, ?)",
                )
                .bind(alert_subscription_id)
                .bind(org_id)
                .bind(env_id)
                .bind(now)
                .bind(now)
                .bind(created_by_user_id)
                .execute(&mut *tx)
                .await?;

                // Link channels
                for channel_id in channel_ids {
                    sqlx::query(
                        "INSERT INTO alert_subscription_channel (subscription_id, channel_id, enabled, created_at)
                         VALUES (?, ?, 1, ?)",
                    )
                    .bind(alert_subscription_id)
                    .bind(channel_id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                }

                // Link destinations
                for destination_id in destination_ids {
                    sqlx::query(
                        "INSERT INTO alert_subscription_destination (subscription_id, destination_id, enabled, created_at)
                         VALUES (?, ?, 1, ?)",
                    )
                    .bind(alert_subscription_id)
                    .bind(destination_id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                }

                tx.commit().await?;
            }
        }

        Self::get_by_id(db, &alert_subscription_id).await
    }

    /// Update a subscription's channels and destinations
    pub async fn update(
        db: &crate::db::DatabasePool,
        alert_subscription_id: &Uuid,
        env_id: Option<&Uuid>,
        channel_ids: &[Uuid],
        destination_ids: &[Uuid],
        enabled: bool,
        updated_by_user_id: &Uuid,
    ) -> Result<AlertSubscription, AlertError> {
        let now = Utc::now();

        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                let mut tx = pool.begin().await?;

                // Update subscription
                sqlx::query(
                    "UPDATE alert_subscription SET env_id = $1, enabled = $2, updated_at = $3, updated_by_user_id = $4
                     WHERE alert_subscription_id = $5",
                )
                .bind(env_id)
                .bind(enabled)
                .bind(now)
                .bind(updated_by_user_id)
                .bind(alert_subscription_id)
                .execute(&mut *tx)
                .await?;

                // Clear and re-add channel links
                sqlx::query("DELETE FROM alert_subscription_channel WHERE subscription_id = $1")
                    .bind(alert_subscription_id)
                    .execute(&mut *tx)
                    .await?;

                for channel_id in channel_ids {
                    sqlx::query(
                        "INSERT INTO alert_subscription_channel (subscription_id, channel_id, enabled, created_at)
                         VALUES ($1, $2, true, $3)",
                    )
                    .bind(alert_subscription_id)
                    .bind(channel_id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                }

                // Clear and re-add destination links
                sqlx::query(
                    "DELETE FROM alert_subscription_destination WHERE subscription_id = $1",
                )
                .bind(alert_subscription_id)
                .execute(&mut *tx)
                .await?;

                for destination_id in destination_ids {
                    sqlx::query(
                        "INSERT INTO alert_subscription_destination (subscription_id, destination_id, enabled, created_at)
                         VALUES ($1, $2, true, $3)",
                    )
                    .bind(alert_subscription_id)
                    .bind(destination_id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                }

                tx.commit().await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                let mut tx = pool.begin().await?;

                // Update subscription
                sqlx::query(
                    "UPDATE alert_subscription SET env_id = ?, enabled = ?, updated_at = ?, updated_by_user_id = ?
                     WHERE alert_subscription_id = ?",
                )
                .bind(env_id)
                .bind(enabled)
                .bind(now)
                .bind(updated_by_user_id)
                .bind(alert_subscription_id)
                .execute(&mut *tx)
                .await?;

                // Clear and re-add channel links
                sqlx::query("DELETE FROM alert_subscription_channel WHERE subscription_id = ?")
                    .bind(alert_subscription_id)
                    .execute(&mut *tx)
                    .await?;

                for channel_id in channel_ids {
                    sqlx::query(
                        "INSERT INTO alert_subscription_channel (subscription_id, channel_id, enabled, created_at)
                         VALUES (?, ?, 1, ?)",
                    )
                    .bind(alert_subscription_id)
                    .bind(channel_id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                }

                // Clear and re-add destination links
                sqlx::query("DELETE FROM alert_subscription_destination WHERE subscription_id = ?")
                    .bind(alert_subscription_id)
                    .execute(&mut *tx)
                    .await?;

                for destination_id in destination_ids {
                    sqlx::query(
                        "INSERT INTO alert_subscription_destination (subscription_id, destination_id, enabled, created_at)
                         VALUES (?, ?, 1, ?)",
                    )
                    .bind(alert_subscription_id)
                    .bind(destination_id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                }

                tx.commit().await?;
            }
        }

        Self::get_by_id(db, alert_subscription_id).await
    }

    /// Delete a subscription
    pub async fn delete(
        db: &crate::db::DatabasePool,
        alert_subscription_id: &Uuid,
    ) -> Result<(), AlertError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query("DELETE FROM alert_subscription WHERE alert_subscription_id = $1")
                    .bind(alert_subscription_id)
                    .execute(pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query("DELETE FROM alert_subscription WHERE alert_subscription_id = ?")
                    .bind(alert_subscription_id)
                    .execute(pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Get channel IDs for a subscription
    pub async fn get_channel_ids(
        db: &crate::db::DatabasePool,
        alert_subscription_id: &Uuid,
    ) -> Result<Vec<Uuid>, AlertError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                let rows: Vec<(Uuid,)> = sqlx::query_as(
                    "SELECT channel_id FROM alert_subscription_channel WHERE subscription_id = $1",
                )
                .bind(alert_subscription_id)
                .fetch_all(pool)
                .await?;
                Ok(rows.into_iter().map(|(id,)| id).collect())
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                let rows: Vec<(Uuid,)> = sqlx::query_as(
                    "SELECT channel_id FROM alert_subscription_channel WHERE subscription_id = ?",
                )
                .bind(alert_subscription_id)
                .fetch_all(pool)
                .await?;
                Ok(rows.into_iter().map(|(id,)| id).collect())
            }
        }
    }

    /// Get destination IDs for a subscription
    pub async fn get_destination_ids(
        db: &crate::db::DatabasePool,
        alert_subscription_id: &Uuid,
    ) -> Result<Vec<Uuid>, AlertError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                let rows: Vec<(Uuid,)> = sqlx::query_as(
                    "SELECT destination_id FROM alert_subscription_destination WHERE subscription_id = $1",
                )
                .bind(alert_subscription_id)
                .fetch_all(pool)
                .await?;
                Ok(rows.into_iter().map(|(id,)| id).collect())
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                let rows: Vec<(Uuid,)> = sqlx::query_as(
                    "SELECT destination_id FROM alert_subscription_destination WHERE subscription_id = ?",
                )
                .bind(alert_subscription_id)
                .fetch_all(pool)
                .await?;
                Ok(rows.into_iter().map(|(id,)| id).collect())
            }
        }
    }
}

// =============================================================================
// Alert Record
// =============================================================================

/// Record of a triggered alert
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Alert {
    pub alert_id: Uuid,
    pub org_id: Uuid,
    pub env_id: Uuid,
    pub channel: String,
    pub data: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

impl Alert {
    /// Create a new alert record
    pub async fn create(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        env_id: &Uuid,
        channel: &str,
        data: &serde_json::Value,
    ) -> Result<Alert, AlertError> {
        let alert_id = Uuid::now_v7();
        let now = Utc::now();

        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO alert (alert_id, org_id, env_id, channel, data, created_at)
                     VALUES ($1, $2, $3, $4, $5, $6)",
                )
                .bind(alert_id)
                .bind(org_id)
                .bind(env_id)
                .bind(channel)
                .bind(data)
                .bind(now)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "INSERT INTO alert (alert_id, org_id, env_id, channel, data, created_at)
                     VALUES (?, ?, ?, ?, ?, ?)",
                )
                .bind(alert_id)
                .bind(org_id)
                .bind(env_id)
                .bind(channel)
                .bind(data.to_string())
                .bind(now)
                .execute(pool)
                .await?;
            }
        }

        Ok(Alert {
            alert_id,
            org_id: *org_id,
            env_id: *env_id,
            channel: channel.to_string(),
            data: data.clone(),
            created_at: now,
        })
    }

    /// Get alerts for an org
    pub async fn get_by_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        limit: i64,
    ) -> Result<Vec<Alert>, AlertError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                let alerts = sqlx::query_as::<_, Alert>(
                    "SELECT * FROM alert WHERE org_id = $1 ORDER BY created_at DESC LIMIT $2",
                )
                .bind(org_id)
                .bind(limit)
                .fetch_all(pool)
                .await?;
                Ok(alerts)
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                let alerts = sqlx::query_as::<_, Alert>(
                    "SELECT * FROM alert WHERE org_id = ? ORDER BY created_at DESC LIMIT ?",
                )
                .bind(org_id)
                .bind(limit)
                .fetch_all(pool)
                .await?;
                Ok(alerts)
            }
        }
    }

    /// Get alerts for an org with pagination
    pub async fn get_by_org_paginated(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Alert>, AlertError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                let alerts = sqlx::query_as::<_, Alert>(
                    "SELECT * FROM alert WHERE org_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
                )
                .bind(org_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pool)
                .await?;
                Ok(alerts)
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                let alerts = sqlx::query_as::<_, Alert>(
                    "SELECT * FROM alert WHERE org_id = ? ORDER BY created_at DESC LIMIT ? OFFSET ?",
                )
                .bind(org_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pool)
                .await?;
                Ok(alerts)
            }
        }
    }

    /// Get total count of alerts for an org
    pub async fn count_by_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
    ) -> Result<i64, AlertError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM alert WHERE org_id = $1")
                    .bind(org_id)
                    .fetch_one(pool)
                    .await?;
                Ok(count.0)
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM alert WHERE org_id = ?")
                    .bind(org_id)
                    .fetch_one(pool)
                    .await?;
                Ok(count.0)
            }
        }
    }

    /// Get alert by ID
    pub async fn get_by_id(
        db: &crate::db::DatabasePool,
        alert_id: &Uuid,
    ) -> Result<Alert, AlertError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query_as::<_, Alert>("SELECT * FROM alert WHERE alert_id = $1")
                    .bind(alert_id)
                    .fetch_optional(pool)
                    .await?
                    .ok_or(AlertError::NotFound)
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query_as::<_, Alert>("SELECT * FROM alert WHERE alert_id = ?")
                    .bind(alert_id)
                    .fetch_optional(pool)
                    .await?
                    .ok_or(AlertError::NotFound)
            }
        }
    }
}

// =============================================================================
// Alert Delivery
// =============================================================================

/// Track delivery attempt for an alert
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct AlertDelivery {
    pub alert_delivery_id: Uuid,
    pub alert_id: Uuid,
    pub subscription_id: Uuid,
    pub destination_id: Uuid,
    pub resolved_user_id: Option<Uuid>,
    pub status_id: i16,
    pub attempts: i32,
    pub max_attempts: i32,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub sent_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl AlertDelivery {
    /// Get delivery status enum
    pub fn status(&self) -> Option<DeliveryStatus> {
        DeliveryStatus::from_i16(self.status_id)
    }

    /// Create a new delivery record
    pub async fn create(
        db: &crate::db::DatabasePool,
        alert_id: &Uuid,
        alert_subscription_id: &Uuid,
        destination_id: &Uuid,
        resolved_user_id: Option<&Uuid>,
    ) -> Result<AlertDelivery, AlertError> {
        let delivery_id = Uuid::now_v7();
        let now = Utc::now();

        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO alert_delivery (alert_delivery_id, alert_id, subscription_id, destination_id, resolved_user_id, created_at)
                     VALUES ($1, $2, $3, $4, $5, $6)",
                )
                .bind(delivery_id)
                .bind(alert_id)
                .bind(alert_subscription_id)
                .bind(destination_id)
                .bind(resolved_user_id)
                .bind(now)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "INSERT INTO alert_delivery (alert_delivery_id, alert_id, subscription_id, destination_id, resolved_user_id, created_at)
                     VALUES (?, ?, ?, ?, ?, ?)",
                )
                .bind(delivery_id)
                .bind(alert_id)
                .bind(alert_subscription_id)
                .bind(destination_id)
                .bind(resolved_user_id)
                .bind(now)
                .execute(pool)
                .await?;
            }
        }

        Ok(AlertDelivery {
            alert_delivery_id: delivery_id,
            alert_id: *alert_id,
            subscription_id: *alert_subscription_id,
            destination_id: *destination_id,
            resolved_user_id: resolved_user_id.copied(),
            status_id: DeliveryStatus::Pending.as_i16(),
            attempts: 0,
            max_attempts: 5,
            next_retry_at: None,
            last_error: None,
            sent_at: None,
            created_at: now,
        })
    }

    /// Mark delivery as sent
    pub async fn mark_sent(
        db: &crate::db::DatabasePool,
        delivery_id: &Uuid,
    ) -> Result<(), AlertError> {
        let now = Utc::now();

        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "UPDATE alert_delivery SET status_id = $1, sent_at = $2, attempts = attempts + 1 WHERE alert_delivery_id = $3",
                )
                .bind(DeliveryStatus::Sent.as_i16())
                .bind(now)
                .bind(delivery_id)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE alert_delivery SET status_id = ?, sent_at = ?, attempts = attempts + 1 WHERE alert_delivery_id = ?",
                )
                .bind(DeliveryStatus::Sent.as_i16())
                .bind(now)
                .bind(delivery_id)
                .execute(pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Mark delivery as failed and schedule retry
    pub async fn mark_failed(
        db: &crate::db::DatabasePool,
        delivery_id: &Uuid,
        error: &str,
        next_retry_at: Option<DateTime<Utc>>,
    ) -> Result<(), AlertError> {
        let status = if next_retry_at.is_some() {
            DeliveryStatus::Retrying
        } else {
            DeliveryStatus::Failed
        };

        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "UPDATE alert_delivery SET status_id = $1, last_error = $2, next_retry_at = $3, attempts = attempts + 1 WHERE alert_delivery_id = $4",
                )
                .bind(status.as_i16())
                .bind(error)
                .bind(next_retry_at)
                .bind(delivery_id)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE alert_delivery SET status_id = ?, last_error = ?, next_retry_at = ?, attempts = attempts + 1 WHERE alert_delivery_id = ?",
                )
                .bind(status.as_i16())
                .bind(error)
                .bind(next_retry_at)
                .bind(delivery_id)
                .execute(pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Get pending deliveries ready for retry
    pub async fn get_pending_retries(
        db: &crate::db::DatabasePool,
        limit: i64,
    ) -> Result<Vec<AlertDelivery>, AlertError> {
        let now = Utc::now();

        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                let deliveries = sqlx::query_as::<_, AlertDelivery>(
                    "SELECT * FROM alert_delivery WHERE status_id IN (1, 4) AND (next_retry_at IS NULL OR next_retry_at <= $1) AND attempts < max_attempts ORDER BY created_at ASC LIMIT $2",
                )
                .bind(now)
                .bind(limit)
                .fetch_all(pool)
                .await?;
                Ok(deliveries)
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                let deliveries = sqlx::query_as::<_, AlertDelivery>(
                    "SELECT * FROM alert_delivery WHERE status_id IN (1, 4) AND (next_retry_at IS NULL OR next_retry_at <= ?) AND attempts < max_attempts ORDER BY created_at ASC LIMIT ?",
                )
                .bind(now)
                .bind(limit)
                .fetch_all(pool)
                .await?;
                Ok(deliveries)
            }
        }
    }

    /// Get delivery by ID with related alert and destination info
    pub async fn get_by_id(
        db: &crate::db::DatabasePool,
        delivery_id: &Uuid,
    ) -> Result<AlertDelivery, AlertError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => sqlx::query_as::<_, AlertDelivery>(
                "SELECT * FROM alert_delivery WHERE alert_delivery_id = $1",
            )
            .bind(delivery_id)
            .fetch_optional(pool)
            .await?
            .ok_or(AlertError::NotFound),
            crate::db::DatabasePool::Sqlite(pool) => sqlx::query_as::<_, AlertDelivery>(
                "SELECT * FROM alert_delivery WHERE alert_delivery_id = ?",
            )
            .bind(delivery_id)
            .fetch_optional(pool)
            .await?
            .ok_or(AlertError::NotFound),
        }
    }

    /// Get all deliveries for an alert
    pub async fn get_by_alert_id(
        db: &crate::db::DatabasePool,
        alert_id: &Uuid,
    ) -> Result<Vec<AlertDelivery>, AlertError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                let deliveries = sqlx::query_as::<_, AlertDelivery>(
                    "SELECT * FROM alert_delivery WHERE alert_id = $1 ORDER BY created_at ASC",
                )
                .bind(alert_id)
                .fetch_all(pool)
                .await?;
                Ok(deliveries)
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                let deliveries = sqlx::query_as::<_, AlertDelivery>(
                    "SELECT * FROM alert_delivery WHERE alert_id = ? ORDER BY created_at ASC",
                )
                .bind(alert_id)
                .fetch_all(pool)
                .await?;
                Ok(deliveries)
            }
        }
    }
}

// =============================================================================
// Alert Publisher
// =============================================================================

/// Publish an alert and create delivery records for matching subscriptions.
///
/// The alert record is always created (for audit/history purposes), even if no
/// channel patterns match. Deliveries are only created if there are matching
/// channels with active subscriptions.
pub async fn publish_alert(
    db: &crate::db::DatabasePool,
    org_id: &Uuid,
    env_id: &Uuid,
    channel: &str,
    data: &serde_json::Value,
) -> Result<Alert, AlertError> {
    // 1. Always create the alert record (for audit trail)
    let alert = Alert::create(db, org_id, env_id, channel, data).await?;

    // 2. Find matching channels (check regex patterns)
    let matching_channels = find_matching_channels(db, org_id, env_id, channel).await?;

    if matching_channels.is_empty() {
        tracing::debug!(
            "Alert '{}' (id: {}) created with no matching channels",
            channel,
            alert.alert_id
        );
        return Ok(alert);
    }

    // 3. Find subscriptions for these channels and create deliveries
    let created_deliveries =
        create_deliveries_for_alert(db, &alert, org_id, env_id, &matching_channels).await?;

    let delivery_count = created_deliveries.len();

    // 4. Enqueue alert delivery messages to the hot:alert queue (if available)
    if !created_deliveries.is_empty() {
        if let Some(queue) = crate::notification_queue::alert_queue() {
            for delivery_info in &created_deliveries {
                let msg = crate::lang::event::queue::AlertDeliveryMessage {
                    id: Uuid::now_v7(),
                    head: ahash::AHashMap::new(),
                    body: crate::lang::event::queue::AlertDeliveryMessageBody {
                        alert_delivery_id: delivery_info.delivery_id,
                        alert_id: alert.alert_id,
                        destination_type: delivery_info.destination_type.clone(),
                    },
                };
                let message: crate::data::msg::Message = msg.into();
                if let Err(e) = crate::queue::Queue::enqueue(queue.as_ref(), message).await {
                    tracing::error!(
                        "Failed to enqueue alert delivery {} to hot:alert queue: {}",
                        delivery_info.delivery_id,
                        e
                    );
                }
            }
            tracing::debug!(
                "Enqueued {} alert deliveries to hot:alert queue for alert {}",
                delivery_count,
                alert.alert_id
            );
        } else {
            tracing::debug!(
                "No alert queue configured, {} deliveries created in DB only (alert {})",
                delivery_count,
                alert.alert_id
            );
        }
    }

    if delivery_count > 0 {
        tracing::info!(
            "Published alert '{}' (id: {}) with {} deliveries",
            channel,
            alert.alert_id,
            delivery_count
        );
    } else {
        tracing::debug!(
            "Published alert '{}' (id: {}) with 0 deliveries",
            channel,
            alert.alert_id
        );
    }

    Ok(alert)
}

/// Find channels that match the given alert channel string
async fn find_matching_channels(
    db: &crate::db::DatabasePool,
    org_id: &Uuid,
    env_id: &Uuid,
    channel: &str,
) -> Result<Vec<AlertChannel>, AlertError> {
    // Get all enabled channels that could match (system + org + env-specific)
    let channels = match db {
        crate::db::DatabasePool::Postgres(pool) => {
            sqlx::query_as::<_, AlertChannel>(
                "SELECT * FROM alert_channel
                 WHERE enabled = true
                 AND (org_id IS NULL OR org_id = $1)
                 AND (env_id IS NULL OR env_id = $2)
                 ORDER BY org_id NULLS FIRST, env_id NULLS FIRST",
            )
            .bind(org_id)
            .bind(env_id)
            .fetch_all(pool)
            .await?
        }
        crate::db::DatabasePool::Sqlite(pool) => {
            sqlx::query_as::<_, AlertChannel>(
                "SELECT * FROM alert_channel
                 WHERE enabled = 1
                 AND (org_id IS NULL OR org_id = ?)
                 AND (env_id IS NULL OR env_id = ?)
                 ORDER BY org_id IS NULL DESC, env_id IS NULL DESC",
            )
            .bind(org_id)
            .bind(env_id)
            .fetch_all(pool)
            .await?
        }
    };

    // Filter to channels whose regex pattern matches the channel string
    // Regexes are cached to avoid recompilation on every alert publish
    let mut matching = Vec::new();
    for ch in channels {
        match get_or_compile_regex(&ch.pattern) {
            Ok(re) => {
                if re.is_match(channel) {
                    matching.push(ch);
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Invalid regex pattern '{}' for channel {}: {}",
                    ch.pattern,
                    ch.name,
                    e
                );
            }
        }
    }

    Ok(matching)
}

/// Info about a created delivery, used for enqueuing to the alert queue
struct CreatedDeliveryInfo {
    delivery_id: Uuid,
    destination_type: String,
}

/// Create delivery records for all subscriptions matching the alert
/// Returns the list of created deliveries with their destination types
///
/// For email destinations with org/team/user targets, this resolves the target
/// to individual users and creates one delivery per user, respecting the user's
/// alert opt-out preference.
async fn create_deliveries_for_alert(
    db: &crate::db::DatabasePool,
    alert: &Alert,
    org_id: &Uuid,
    env_id: &Uuid,
    channels: &[AlertChannel],
) -> Result<Vec<CreatedDeliveryInfo>, AlertError> {
    let channel_ids: Vec<Uuid> = channels.iter().map(|c| c.alert_channel_id).collect();

    if channel_ids.is_empty() {
        return Ok(Vec::new());
    }

    // Find subscriptions that:
    // 1. Are enabled
    // 2. Match any of the channels
    // 3. Scope matches (org-wide or env-specific)
    // Also fetch destination_type_id and config for email target resolution
    let subscriptions_with_destinations: Vec<(Uuid, Uuid, i16, serde_json::Value)> = match db {
        crate::db::DatabasePool::Postgres(pool) => {
            let query = "SELECT DISTINCT s.alert_subscription_id, sd.destination_id, d.destination_type_id, d.config
                 FROM alert_subscription s
                 JOIN alert_subscription_channel sc ON s.alert_subscription_id = sc.subscription_id
                 JOIN alert_subscription_destination sd ON s.alert_subscription_id = sd.subscription_id
                 JOIN alert_destination d ON sd.destination_id = d.alert_destination_id
                 WHERE s.enabled = true
                 AND sc.enabled = true
                 AND sd.enabled = true
                 AND d.enabled = true
                 AND d.verified = true
                 AND s.org_id = $1
                 AND (s.env_id IS NULL OR s.env_id = $2)
                 AND sc.channel_id = ANY($3)".to_string();
            sqlx::query_as(sqlx::AssertSqlSafe(query.as_str()))
                .bind(org_id)
                .bind(env_id)
                .bind(&channel_ids)
                .fetch_all(pool)
                .await?
        }
        crate::db::DatabasePool::Sqlite(pool) => {
            let placeholders: Vec<String> =
                (0..channel_ids.len()).map(|_| "?".to_string()).collect();
            let query = format!(
                "SELECT DISTINCT s.alert_subscription_id, sd.destination_id, d.destination_type_id, d.config
                 FROM alert_subscription s
                 JOIN alert_subscription_channel sc ON s.alert_subscription_id = sc.subscription_id
                 JOIN alert_subscription_destination sd ON s.alert_subscription_id = sd.subscription_id
                 JOIN alert_destination d ON sd.destination_id = d.alert_destination_id
                 WHERE s.enabled = 1
                 AND sc.enabled = 1
                 AND sd.enabled = 1
                 AND d.enabled = 1
                 AND d.verified = 1
                 AND s.org_id = ?
                 AND (s.env_id IS NULL OR s.env_id = ?)
                 AND sc.channel_id IN ({})",
                placeholders.join(", ")
            );
            let mut q = sqlx::query_as(sqlx::AssertSqlSafe(query.as_str()))
                .bind(org_id)
                .bind(env_id);
            for id in &channel_ids {
                q = q.bind(id);
            }
            q.fetch_all(pool).await?
        }
    };

    // Create delivery records and collect info for queue enqueuing.
    // Track seen email addresses (lowercased) to deduplicate across all email
    // destinations for this alert -- a user should receive at most one email
    // per alert regardless of how many destinations resolve to their address.
    let mut created = Vec::new();
    let mut seen_emails: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (alert_subscription_id, destination_id, destination_type_id, config) in
        subscriptions_with_destinations
    {
        let dest_type = DestinationType::from_i16(destination_type_id)
            .map(|dt| dt.as_str().to_string())
            .unwrap_or_else(|| format!("unknown_{}", destination_type_id));

        // For email destinations, check if we need to resolve org/team/user targets
        if destination_type_id == DestinationType::Email.as_i16()
            && let Ok(email_config) = EmailDestinationConfig::from_config(&config)
        {
            match &email_config.target {
                EmailTarget::Address { address } => {
                    // Static address: deduplicate by lowercased email
                    let email_key = address.to_lowercase();
                    if !seen_emails.insert(email_key) {
                        tracing::debug!(
                            "Skipping duplicate email delivery to '{}' for alert {}",
                            address,
                            alert.alert_id
                        );
                        continue;
                    }
                    match AlertDelivery::create(
                        db,
                        &alert.alert_id,
                        &alert_subscription_id,
                        &destination_id,
                        None,
                    )
                    .await
                    {
                        Ok(delivery) => {
                            created.push(CreatedDeliveryInfo {
                                delivery_id: delivery.alert_delivery_id,
                                destination_type: dest_type.clone(),
                            });
                        }
                        Err(e) => {
                            tracing::error!(
                                "Failed to create delivery for alert {} to destination {}: {}",
                                alert.alert_id,
                                destination_id,
                                e
                            );
                        }
                    }
                }
                EmailTarget::Org => {
                    // Resolve all active org members who haven't opted out
                    match crate::db::user::User::get_alert_recipients_by_org(db, org_id).await {
                        Ok(recipients) => {
                            for (user_id, email) in &recipients {
                                let email_key = email.to_lowercase();
                                if !seen_emails.insert(email_key) {
                                    tracing::debug!(
                                        "Skipping duplicate email delivery to '{}' (user {}) for alert {}",
                                        email,
                                        user_id,
                                        alert.alert_id
                                    );
                                    continue;
                                }
                                match AlertDelivery::create(
                                    db,
                                    &alert.alert_id,
                                    &alert_subscription_id,
                                    &destination_id,
                                    Some(user_id),
                                )
                                .await
                                {
                                    Ok(delivery) => {
                                        created.push(CreatedDeliveryInfo {
                                            delivery_id: delivery.alert_delivery_id,
                                            destination_type: dest_type.clone(),
                                        });
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            "Failed to create delivery for alert {} to user {}: {}",
                                            alert.alert_id,
                                            user_id,
                                            e
                                        );
                                    }
                                }
                            }
                            if recipients.is_empty() {
                                tracing::debug!(
                                    "No alert recipients found for org {} (all opted out or inactive)",
                                    org_id
                                );
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                "Failed to resolve org email recipients for alert {}: {}",
                                alert.alert_id,
                                e
                            );
                        }
                    }
                }
                EmailTarget::Team { team_id } => {
                    // Resolve all active team members who haven't opted out
                    match crate::db::user::User::get_alert_recipients_by_team(db, team_id).await {
                        Ok(recipients) => {
                            for (user_id, email) in &recipients {
                                let email_key = email.to_lowercase();
                                if !seen_emails.insert(email_key) {
                                    tracing::debug!(
                                        "Skipping duplicate email delivery to '{}' (user {}) for alert {}",
                                        email,
                                        user_id,
                                        alert.alert_id
                                    );
                                    continue;
                                }
                                match AlertDelivery::create(
                                    db,
                                    &alert.alert_id,
                                    &alert_subscription_id,
                                    &destination_id,
                                    Some(user_id),
                                )
                                .await
                                {
                                    Ok(delivery) => {
                                        created.push(CreatedDeliveryInfo {
                                            delivery_id: delivery.alert_delivery_id,
                                            destination_type: dest_type.clone(),
                                        });
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            "Failed to create delivery for alert {} to user {}: {}",
                                            alert.alert_id,
                                            user_id,
                                            e
                                        );
                                    }
                                }
                            }
                            if recipients.is_empty() {
                                tracing::debug!(
                                    "No alert recipients found for team {} (all opted out or inactive)",
                                    team_id
                                );
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                "Failed to resolve team email recipients for alert {}: {}",
                                alert.alert_id,
                                e
                            );
                        }
                    }
                }
                EmailTarget::User { user_id } => {
                    // Check if user has opted out
                    match crate::db::user::User::get_user(db, user_id).await {
                        Ok(user) if user.active && user.alerts_enabled() => {
                            let email_key = user.email.to_lowercase();
                            if !seen_emails.insert(email_key) {
                                tracing::debug!(
                                    "Skipping duplicate email delivery to '{}' (user {}) for alert {}",
                                    user.email,
                                    user_id,
                                    alert.alert_id
                                );
                                continue;
                            }
                            match AlertDelivery::create(
                                db,
                                &alert.alert_id,
                                &alert_subscription_id,
                                &destination_id,
                                Some(user_id),
                            )
                            .await
                            {
                                Ok(delivery) => {
                                    created.push(CreatedDeliveryInfo {
                                        delivery_id: delivery.alert_delivery_id,
                                        destination_type: dest_type.clone(),
                                    });
                                }
                                Err(e) => {
                                    tracing::error!(
                                        "Failed to create delivery for alert {} to user {}: {}",
                                        alert.alert_id,
                                        user_id,
                                        e
                                    );
                                }
                            }
                        }
                        Ok(user) => {
                            tracing::debug!(
                                "Skipping alert delivery to user {} (active={}, alerts_enabled={})",
                                user_id,
                                user.active,
                                user.alerts_enabled()
                            );
                        }
                        Err(e) => {
                            tracing::error!(
                                "Failed to resolve user {} for alert {}: {}",
                                user_id,
                                alert.alert_id,
                                e
                            );
                        }
                    }
                }
            }
            continue;
        }

        // Non-email destinations (or email with unparseable config): create one delivery
        match AlertDelivery::create(
            db,
            &alert.alert_id,
            &alert_subscription_id,
            &destination_id,
            None,
        )
        .await
        {
            Ok(delivery) => {
                created.push(CreatedDeliveryInfo {
                    delivery_id: delivery.alert_delivery_id,
                    destination_type: dest_type,
                });
            }
            Err(e) => {
                tracing::error!(
                    "Failed to create delivery for alert {} to destination {}: {}",
                    alert.alert_id,
                    destination_id,
                    e
                );
            }
        }
    }

    Ok(created)
}

// =============================================================================
// Alert Delivery Processor
// =============================================================================

/// Result of processing a single delivery
#[derive(Debug)]
pub enum DeliveryResult {
    /// Successfully delivered
    Success,
    /// Delivery failed, will retry
    Retry(String),
    /// Delivery failed permanently
    PermanentFailure(String),
}

/// Email destination target type
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum EmailTarget {
    /// Send to a specific email address (e.g., shared inbox, external partner)
    Address { address: String },
    /// Send to all active members of the org
    Org,
    /// Send to all active members of a specific team
    Team { team_id: Uuid },
    /// Send to a specific user (by user_id, resolves to their current email)
    User { user_id: Uuid },
}

/// Configuration for an email destination
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailDestinationConfig {
    /// The email target (address, org, team, or user)
    #[serde(flatten)]
    pub target: EmailTarget,
}

impl EmailDestinationConfig {
    /// Parse from JSON config, with backward compatibility for old `{ "address": "..." }` format
    pub fn from_config(config: &serde_json::Value) -> Result<Self, String> {
        // Try new tagged format first
        if let Ok(cfg) = serde_json::from_value::<EmailDestinationConfig>(config.clone()) {
            return Ok(cfg);
        }
        // Fall back to old format: { "address": "email@example.com" } without target tag
        if let Some(address) = config.get("address").and_then(|v| v.as_str()) {
            return Ok(EmailDestinationConfig {
                target: EmailTarget::Address {
                    address: address.to_string(),
                },
            });
        }
        Err("Invalid email destination config".to_string())
    }

    /// Human-readable description of the email target
    pub fn target_description(&self) -> String {
        match &self.target {
            EmailTarget::Address { address } => address.clone(),
            EmailTarget::Org => "Everyone in Org".to_string(),
            EmailTarget::Team { .. } => "Team".to_string(),
            EmailTarget::User { .. } => "User".to_string(),
        }
    }
}

/// Configuration for a Slack destination
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlackDestinationConfig {
    pub webhook_url: String,
    pub channel: Option<String>,
}

/// Configuration for a PagerDuty destination
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PagerDutyDestinationConfig {
    pub routing_key: String,
    pub severity: Option<String>, // critical, error, warning, info
}

/// Configuration for a webhook destination
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookDestinationConfig {
    pub url: String,
    pub headers: Option<std::collections::HashMap<String, String>>,
}

/// Get delivery info needed for processing
#[derive(Debug)]
pub struct DeliveryInfo {
    pub delivery: AlertDelivery,
    pub alert: Alert,
    pub destination: AlertDestination,
}

impl DeliveryInfo {
    /// Fetch all info needed to process a delivery
    pub async fn fetch(
        db: &crate::db::DatabasePool,
        delivery: AlertDelivery,
    ) -> Result<Self, AlertError> {
        let alert = Alert::get_by_id(db, &delivery.alert_id).await?;
        let destination = AlertDestination::get_by_id(db, &delivery.destination_id).await?;

        Ok(Self {
            delivery,
            alert,
            destination,
        })
    }
}

/// Process a single alert delivery by ID (called by queue consumers)
///
/// Fetches the delivery info from DB, sends it, and updates status.
/// Returns true if successful, false if failed.
pub async fn process_single_alert_delivery(
    db: &crate::db::DatabasePool,
    http_client: &reqwest::Client,
    email_sender: Option<&dyn AlertEmailSender>,
    email_config: &crate::email::EmailConfig,
    alert_delivery_id: &Uuid,
) -> Result<bool, AlertError> {
    // Fetch the delivery record
    let delivery = AlertDelivery::get_by_id(db, alert_delivery_id).await?;

    // Fetch full delivery info (alert + destination)
    let info = DeliveryInfo::fetch(db, delivery).await?;

    // Process the delivery
    let result = process_delivery(&info, http_client, email_sender, email_config, db).await;

    // Update delivery status
    match result {
        DeliveryResult::Success => {
            AlertDelivery::mark_sent(db, alert_delivery_id).await?;
            Ok(true)
        }
        DeliveryResult::Retry(error) => {
            let next_attempt = info.delivery.attempts + 1;
            let next_retry = if next_attempt < info.delivery.max_attempts {
                let delay_secs = 60 * (2_i64.pow(next_attempt as u32)).min(3600);
                Some(Utc::now() + chrono::Duration::seconds(delay_secs))
            } else {
                None
            };
            AlertDelivery::mark_failed(db, alert_delivery_id, &error, next_retry).await?;
            Ok(false)
        }
        DeliveryResult::PermanentFailure(error) => {
            AlertDelivery::mark_failed(db, alert_delivery_id, &error, None).await?;
            Ok(false)
        }
    }
}

/// Process pending alert deliveries (recovery sweep for orphaned deliveries)
/// Returns (processed_count, success_count, failure_count)
pub async fn process_pending_deliveries(
    db: &crate::db::DatabasePool,
    http_client: &reqwest::Client,
    email_sender: Option<&dyn AlertEmailSender>,
    email_config: &crate::email::EmailConfig,
    batch_size: i64,
) -> Result<(usize, usize, usize), AlertError> {
    let pending = AlertDelivery::get_pending_retries(db, batch_size).await?;
    let pending_count = pending.len();

    if pending_count == 0 {
        return Ok((0, 0, 0));
    }

    tracing::debug!("Processing {} pending alert deliveries", pending_count);

    let mut success_count = 0;
    let mut failure_count = 0;

    for delivery in pending {
        let delivery_id = delivery.alert_delivery_id;

        // Fetch full delivery info
        let info = match DeliveryInfo::fetch(db, delivery).await {
            Ok(info) => info,
            Err(e) => {
                tracing::error!("Failed to fetch delivery info for {}: {}", delivery_id, e);
                failure_count += 1;
                continue;
            }
        };

        // Process the delivery
        let result = process_delivery(&info, http_client, email_sender, email_config, db).await;

        // Update delivery status
        match result {
            DeliveryResult::Success => {
                if let Err(e) = AlertDelivery::mark_sent(db, &delivery_id).await {
                    tracing::error!("Failed to mark delivery {} as sent: {}", delivery_id, e);
                }
                success_count += 1;
            }
            DeliveryResult::Retry(error) => {
                // Calculate next retry time (exponential backoff)
                let next_attempt = info.delivery.attempts + 1;
                let next_retry = if next_attempt < info.delivery.max_attempts {
                    let delay_secs = 60 * (2_i64.pow(next_attempt as u32)).min(3600); // Cap at 1 hour
                    Some(Utc::now() + chrono::Duration::seconds(delay_secs))
                } else {
                    None // Max attempts reached
                };

                if let Err(e) =
                    AlertDelivery::mark_failed(db, &delivery_id, &error, next_retry).await
                {
                    tracing::error!("Failed to mark delivery {} as failed: {}", delivery_id, e);
                }
                failure_count += 1;
            }
            DeliveryResult::PermanentFailure(error) => {
                if let Err(e) = AlertDelivery::mark_failed(db, &delivery_id, &error, None).await {
                    tracing::error!(
                        "Failed to mark delivery {} as permanently failed: {}",
                        delivery_id,
                        e
                    );
                }
                failure_count += 1;
            }
        }
    }

    Ok((pending_count, success_count, failure_count))
}

/// Trait for sending alert emails (to decouple from hot_app)
#[async_trait::async_trait]
pub trait AlertEmailSender: Send + Sync {
    async fn send_alert_email(&self, to: &str, subject: &str, html: &str) -> Result<(), String>;
}

/// Process a single delivery based on destination type
async fn process_delivery(
    info: &DeliveryInfo,
    http_client: &reqwest::Client,
    email_sender: Option<&dyn AlertEmailSender>,
    email_config: &crate::email::EmailConfig,
    db: &crate::db::DatabasePool,
) -> DeliveryResult {
    let dest_type = match info.destination.destination_type() {
        Some(dt) => dt,
        None => {
            return DeliveryResult::PermanentFailure(format!(
                "Unknown destination type: {}",
                info.destination.destination_type_id
            ));
        }
    };

    match dest_type {
        DestinationType::Email => {
            process_email_delivery(info, email_sender, email_config, db).await
        }
        DestinationType::Slack => process_slack_delivery(info, http_client).await,
        DestinationType::PagerDuty => process_pagerduty_delivery(info, http_client).await,
        DestinationType::Webhook => process_webhook_delivery(info, http_client).await,
    }
}

/// Process email delivery
///
/// If `resolved_user_id` is set on the delivery, look up the user's current
/// email address and send to that. Otherwise, use the destination config address.
async fn process_email_delivery(
    info: &DeliveryInfo,
    email_sender: Option<&dyn AlertEmailSender>,
    email_config: &crate::email::EmailConfig,
    db: &crate::db::DatabasePool,
) -> DeliveryResult {
    let sender = match email_sender {
        Some(s) => s,
        None => return DeliveryResult::PermanentFailure("Email sender not configured".to_string()),
    };

    // Determine the recipient email address
    let to_address = if let Some(user_id) = &info.delivery.resolved_user_id {
        // Dynamic target: look up user's current email
        match crate::db::user::User::get_user(db, user_id).await {
            Ok(user) => {
                if !user.active {
                    return DeliveryResult::PermanentFailure(format!(
                        "User {} is no longer active",
                        user_id
                    ));
                }
                if !user.alerts_enabled() {
                    return DeliveryResult::PermanentFailure(format!(
                        "User {} has opted out of alert emails",
                        user_id
                    ));
                }
                user.email
            }
            Err(e) => {
                return DeliveryResult::PermanentFailure(format!(
                    "Failed to resolve user {}: {}",
                    user_id, e
                ));
            }
        }
    } else {
        // Static address: use destination config
        let config = match EmailDestinationConfig::from_config(&info.destination.config) {
            Ok(c) => c,
            Err(e) => {
                return DeliveryResult::PermanentFailure(format!("Invalid email config: {}", e));
            }
        };
        match config.target {
            EmailTarget::Address { address } => address,
            _ => {
                return DeliveryResult::PermanentFailure(
                    "Dynamic email target without resolved_user_id".to_string(),
                );
            }
        }
    };

    // Build email content
    let subject = format!("[Hot Dev Alert] {}", info.alert.channel);
    let html = build_alert_email_html(&info.alert, email_config);

    match sender.send_alert_email(&to_address, &subject, &html).await {
        Ok(()) => DeliveryResult::Success,
        Err(e) => DeliveryResult::Retry(e),
    }
}

/// Process Slack webhook delivery
async fn process_slack_delivery(
    info: &DeliveryInfo,
    http_client: &reqwest::Client,
) -> DeliveryResult {
    let config: SlackDestinationConfig =
        match serde_json::from_value(info.destination.config.clone()) {
            Ok(c) => c,
            Err(e) => {
                return DeliveryResult::PermanentFailure(format!("Invalid Slack config: {}", e));
            }
        };

    // Build Slack message
    let blocks = vec![
        serde_json::json!({
            "type": "header",
            "text": {
                "type": "plain_text",
                "text": format!("🚨 Alert: {}", info.alert.channel),
                "emoji": true
            }
        }),
        serde_json::json!({
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": format_alert_data_for_slack(&info.alert.data)
            }
        }),
    ];

    // Add channel override if specified
    let mut payload = serde_json::json!({
        "blocks": blocks
    });

    if let Some(channel) = &config.channel {
        payload["channel"] = serde_json::json!(channel);
    }

    match http_client
        .post(&config.webhook_url)
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) => {
            if resp.status().is_success() {
                DeliveryResult::Success
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                DeliveryResult::Retry(format!("Slack returned {}: {}", status, body))
            }
        }
        Err(e) => DeliveryResult::Retry(format!("Slack request failed: {}", e)),
    }
}

/// Process PagerDuty event delivery
async fn process_pagerduty_delivery(
    info: &DeliveryInfo,
    http_client: &reqwest::Client,
) -> DeliveryResult {
    let config: PagerDutyDestinationConfig =
        match serde_json::from_value(info.destination.config.clone()) {
            Ok(c) => c,
            Err(e) => {
                return DeliveryResult::PermanentFailure(format!(
                    "Invalid PagerDuty config: {}",
                    e
                ));
            }
        };

    let severity = config.severity.as_deref().unwrap_or("error");

    // Build PagerDuty Events API v2 payload
    let payload = serde_json::json!({
        "routing_key": config.routing_key,
        "event_action": "trigger",
        "dedup_key": info.alert.alert_id.to_string(),
        "payload": {
            "summary": format!("Hot Dev Alert: {}", info.alert.channel),
            "source": "hot.dev",
            "severity": severity,
            "custom_details": info.alert.data
        }
    });

    match http_client
        .post("https://events.pagerduty.com/v2/enqueue")
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) => {
            if resp.status().is_success() {
                DeliveryResult::Success
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                if status.as_u16() == 429 {
                    DeliveryResult::Retry(format!("PagerDuty rate limited: {}", body))
                } else if status.is_client_error() {
                    DeliveryResult::PermanentFailure(format!(
                        "PagerDuty returned {}: {}",
                        status, body
                    ))
                } else {
                    DeliveryResult::Retry(format!("PagerDuty returned {}: {}", status, body))
                }
            }
        }
        Err(e) => DeliveryResult::Retry(format!("PagerDuty request failed: {}", e)),
    }
}

/// Process generic webhook delivery
async fn process_webhook_delivery(
    info: &DeliveryInfo,
    http_client: &reqwest::Client,
) -> DeliveryResult {
    let config: WebhookDestinationConfig =
        match serde_json::from_value(info.destination.config.clone()) {
            Ok(c) => c,
            Err(e) => {
                return DeliveryResult::PermanentFailure(format!("Invalid webhook config: {}", e));
            }
        };

    // Build webhook payload
    let payload = serde_json::json!({
        "alert_id": info.alert.alert_id.to_string(),
        "channel": info.alert.channel,
        "data": info.alert.data,
        "timestamp": info.alert.created_at.to_rfc3339(),
    });

    let mut request = http_client.post(&config.url).json(&payload);

    // Add custom headers if specified
    if let Some(headers) = &config.headers {
        for (key, value) in headers {
            request = request.header(key, value);
        }
    }

    match request.send().await {
        Ok(resp) => {
            if resp.status().is_success() {
                DeliveryResult::Success
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                if status.is_client_error() && status.as_u16() != 429 {
                    DeliveryResult::PermanentFailure(format!(
                        "Webhook returned {}: {}",
                        status, body
                    ))
                } else {
                    DeliveryResult::Retry(format!("Webhook returned {}: {}", status, body))
                }
            }
        }
        Err(e) => DeliveryResult::Retry(format!("Webhook request failed: {}", e)),
    }
}

/// Simple HTML escape function
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Build HTML content for alert email
fn build_alert_email_html(alert: &Alert, email_config: &crate::email::EmailConfig) -> String {
    let data_pretty = serde_json::to_string_pretty(&alert.data).unwrap_or_default();
    let base = email_config.app_base_url.trim_end_matches('/');
    let alert_detail_url = format!("{}/settings/alerts/history/{}", base, alert.alert_id);
    let web_url = email_config.web_base_url.as_str();
    let logo_url = email_config.logo_url.as_str();

    // Build contextual quick-links based on channel type
    let mut quick_links = Vec::new();
    if alert.channel.starts_with("run:") {
        if let Some(run_id) = alert.data.get("run_id").and_then(|v| v.as_str()) {
            quick_links.push(format!(
                r#"<a href="{}/runs/{}" style="color: #3b82f6; text-decoration: none; font-weight: 500;">View Run &rarr;</a>"#,
                base, run_id
            ));
        }
    } else if alert.channel.starts_with("deploy:")
        && let (Some(project_name), Some(build_id)) = (
            alert.data.get("project_name").and_then(|v| v.as_str()),
            alert.data.get("build_id").and_then(|v| v.as_str()),
        )
    {
        quick_links.push(format!(
                r#"<a href="{}/projects/{}/builds?build={}" style="color: #3b82f6; text-decoration: none; font-weight: 500;">View Deploy (build {}) &rarr;</a>"#,
                base,
                percent_encode(project_name),
                build_id,
                &build_id[..8.min(build_id.len())]
            ));
    }

    let quick_links_html = if quick_links.is_empty() {
        String::new()
    } else {
        format!(
            r#"
    <div style="text-align: center; margin-bottom: 20px;">
        <p style="font-size: 14px;">{}</p>
    </div>"#,
            quick_links.join(" &nbsp;&middot;&nbsp; ")
        )
    };

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Hot Dev Alert: {channel}</title>
</head>
<body style="font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif; line-height: 1.6; color: #333; max-width: 600px; margin: 0 auto; padding: 20px;">
    <div style="text-align: center; margin-bottom: 30px;">
        <a href="{web_url}" style="text-decoration: none;">
            <img src="{logo_url}" alt="Hot Dev" width="200" style="height: auto;" />
        </a>
    </div>

    <div style="background: #fef2f2; border: 1px solid #fecaca; border-radius: 8px; padding: 20px; margin-bottom: 20px;">
        <h2 style="margin-top: 0; color: #dc2626;">🚨 {channel}</h2>
        <p style="color: #7f1d1d;">An alert was triggered in your Hot environment.</p>
    </div>

    <div style="background: #f7fafc; border-radius: 8px; padding: 20px; margin-bottom: 20px;">
        <h3 style="margin-top: 0; color: #2d3748;">Alert Details</h3>
        <pre style="background: #1a202c; color: #e2e8f0; padding: 15px; border-radius: 4px; overflow-x: auto; font-size: 13px;">{data}</pre>
    </div>
{quick_links}
    <div style="text-align: center; margin-bottom: 20px;">
        <a href="{alert_detail_url}" style="display: inline-block; padding: 10px 24px; background-color: #dc2626; color: #ffffff; text-decoration: none; border-radius: 6px; font-size: 14px; font-weight: 500;">View Alert Details</a>
    </div>

    <div style="text-align: center; color: #a0aec0; font-size: 12px; margin-top: 30px;">
        <p>This alert was sent from <a href="{web_url}" style="color: #e53e3e;">Hot Dev</a></p>
        <p>Alert ID: {alert_id}</p>
    </div>
</body>
</html>"#,
        channel = escape_html(&alert.channel),
        data = escape_html(&data_pretty),
        quick_links = quick_links_html,
        alert_detail_url = alert_detail_url,
        alert_id = alert.alert_id,
        web_url = web_url,
        logo_url = logo_url,
    )
}

/// Percent-encode a string for use in a URL path segment
fn percent_encode(s: &str) -> String {
    use std::fmt::Write;
    let mut encoded = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                write!(encoded, "%{:02X}", byte).unwrap();
            }
        }
    }
    encoded
}

/// Format alert data for Slack message
fn format_alert_data_for_slack(data: &serde_json::Value) -> String {
    if let Some(obj) = data.as_object() {
        let mut lines = Vec::new();
        for (key, value) in obj {
            let value_str = match value {
                serde_json::Value::String(s) => s.clone(),
                _ => value.to_string(),
            };
            lines.push(format!("*{}:* {}", key, value_str));
        }
        lines.join("\n")
    } else {
        data.to_string()
    }
}

// =============================================================================
// Deploy Alert Helpers
// =============================================================================

/// Publish a deploy:succeeded alert (fire-and-forget)
pub async fn publish_deploy_succeeded_alert(
    db: &crate::db::DatabasePool,
    org_id: &Uuid,
    env_id: &Uuid,
    build_id: &Uuid,
    project_name: &str,
) {
    let data = serde_json::json!({
        "build_id": build_id.to_string(),
        "env_id": env_id.to_string(),
        "project_name": project_name,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });

    match publish_alert(db, org_id, env_id, "deploy:succeeded", &data).await {
        Ok(alert) => {
            tracing::debug!(
                "Published deploy:succeeded alert {} for build {}",
                alert.alert_id,
                build_id
            );
        }
        Err(e) => {
            tracing::error!(
                "Failed to publish deploy:succeeded alert for build {}: {}",
                build_id,
                e
            );
        }
    }
}

/// Publish a deploy:failed alert (fire-and-forget)
pub async fn publish_deploy_failed_alert(
    db: &crate::db::DatabasePool,
    org_id: &Uuid,
    env_id: &Uuid,
    build_id: &Uuid,
    project_name: &str,
    error: &str,
) {
    let data = serde_json::json!({
        "build_id": build_id.to_string(),
        "env_id": env_id.to_string(),
        "project_name": project_name,
        "error": error,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });

    match publish_alert(db, org_id, env_id, "deploy:failed", &data).await {
        Ok(alert) => {
            tracing::debug!(
                "Published deploy:failed alert {} for build {}",
                alert.alert_id,
                build_id
            );
        }
        Err(e) => {
            tracing::error!(
                "Failed to publish deploy:failed alert for build {}: {}",
                build_id,
                e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_test_db() -> crate::db::DatabasePool {
        crate::db::test_db().await
    }

    fn test_org_id() -> Uuid {
        Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap()
    }

    fn test_user_id() -> Uuid {
        Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap()
    }

    #[tokio::test]
    async fn test_alert_subscription_uses_subscription_id_mappings() {
        let db = setup_test_db().await;
        let channel_id = Uuid::now_v7();
        let destination_id = Uuid::now_v7();

        let subscription = AlertSubscription::create(
            &db,
            &test_org_id(),
            None,
            &[channel_id],
            &[destination_id],
            &test_user_id(),
        )
        .await
        .unwrap();

        assert_eq!(
            AlertSubscription::get_channel_ids(&db, &subscription.alert_subscription_id)
                .await
                .unwrap(),
            vec![channel_id]
        );
        assert_eq!(
            AlertSubscription::get_destination_ids(&db, &subscription.alert_subscription_id)
                .await
                .unwrap(),
            vec![destination_id]
        );

        let replacement_channel_id = Uuid::now_v7();
        let replacement_destination_id = Uuid::now_v7();
        let updated = AlertSubscription::update(
            &db,
            &subscription.alert_subscription_id,
            None,
            &[replacement_channel_id],
            &[replacement_destination_id],
            true,
            &test_user_id(),
        )
        .await
        .unwrap();

        assert_eq!(
            updated.alert_subscription_id,
            subscription.alert_subscription_id
        );
        assert_eq!(
            AlertSubscription::get_channel_ids(&db, &subscription.alert_subscription_id)
                .await
                .unwrap(),
            vec![replacement_channel_id]
        );
        assert_eq!(
            AlertSubscription::get_destination_ids(&db, &subscription.alert_subscription_id)
                .await
                .unwrap(),
            vec![replacement_destination_id]
        );
    }

    #[tokio::test]
    async fn test_alert_delivery_maps_subscription_id() {
        let db = setup_test_db().await;
        let alert = Alert::create(
            &db,
            &test_org_id(),
            &Uuid::now_v7(),
            "run:failed",
            &serde_json::json!({"run_id": Uuid::now_v7()}),
        )
        .await
        .unwrap();
        let alert_subscription_id = Uuid::now_v7();
        let destination_id = Uuid::now_v7();

        let delivery = AlertDelivery::create(
            &db,
            &alert.alert_id,
            &alert_subscription_id,
            &destination_id,
            None,
        )
        .await
        .unwrap();

        assert_eq!(delivery.subscription_id, alert_subscription_id);
        let fetched = AlertDelivery::get_by_id(&db, &delivery.alert_delivery_id)
            .await
            .unwrap();
        assert_eq!(fetched.subscription_id, alert_subscription_id);

        let deliveries = AlertDelivery::get_by_alert_id(&db, &alert.alert_id)
            .await
            .unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].subscription_id, alert_subscription_id);
    }

    #[tokio::test]
    async fn test_create_with_verification_unverified() {
        let db = setup_test_db().await;
        let token = AlertDestination::generate_verification_token();
        let expires_at = Utc::now() + chrono::Duration::hours(24);
        let config = serde_json::json!({"target": "address", "address": "ext@example.com"});

        let dest = AlertDestination::create_with_verification(
            &db,
            &test_org_id(),
            "External Alerts",
            DestinationType::Email,
            &config,
            &test_user_id(),
            false,
            Some(&token),
            Some(expires_at),
        )
        .await
        .unwrap();

        assert!(!dest.verified, "destination should be unverified");
        assert_eq!(dest.verification_token, Some(token));
        assert!(dest.verification_expires_at.is_some());
        assert_eq!(dest.verification_attempts, 0);
        assert_eq!(dest.name, "External Alerts");
    }

    #[tokio::test]
    async fn test_create_auto_verified() {
        let db = setup_test_db().await;
        let config = serde_json::json!({"target": "org"});

        let dest = AlertDestination::create(
            &db,
            &test_org_id(),
            "Org Email",
            DestinationType::Email,
            &config,
            &test_user_id(),
        )
        .await
        .unwrap();

        assert!(
            dest.verified,
            "non-address destinations should be auto-verified"
        );
        assert!(dest.verification_token.is_none());
    }

    #[tokio::test]
    async fn test_verify_by_token_success() {
        let db = setup_test_db().await;
        let token = AlertDestination::generate_verification_token();
        let expires_at = Utc::now() + chrono::Duration::hours(24);
        let config = serde_json::json!({"target": "address", "address": "ext@example.com"});

        let dest = AlertDestination::create_with_verification(
            &db,
            &test_org_id(),
            "To Verify",
            DestinationType::Email,
            &config,
            &test_user_id(),
            false,
            Some(&token),
            Some(expires_at),
        )
        .await
        .unwrap();
        assert!(!dest.verified);

        // Verify by token
        let verified = AlertDestination::verify_by_token(&db, &token)
            .await
            .unwrap();
        assert!(verified.verified, "destination should now be verified");
        assert!(
            verified.verification_token.is_none(),
            "token should be cleared"
        );
        assert!(
            verified.verification_expires_at.is_none(),
            "expires_at should be cleared"
        );
    }

    #[tokio::test]
    async fn test_verify_by_token_invalid_token() {
        let db = setup_test_db().await;

        let result = AlertDestination::verify_by_token(&db, "nonexistent-token").await;
        assert!(matches!(result, Err(AlertError::NotFound)));
    }

    #[tokio::test]
    async fn test_verify_by_token_expired() {
        let db = setup_test_db().await;
        let token = AlertDestination::generate_verification_token();
        // Expired 1 hour ago
        let expires_at = Utc::now() - chrono::Duration::hours(1);
        let config = serde_json::json!({"target": "address", "address": "ext@example.com"});

        AlertDestination::create_with_verification(
            &db,
            &test_org_id(),
            "Expired",
            DestinationType::Email,
            &config,
            &test_user_id(),
            false,
            Some(&token),
            Some(expires_at),
        )
        .await
        .unwrap();

        let result = AlertDestination::verify_by_token(&db, &token).await;
        assert!(result.is_err(), "expired token should fail");
        assert!(
            result.unwrap_err().to_string().contains("expired"),
            "error should mention expiration"
        );
    }

    #[tokio::test]
    async fn test_verify_by_token_already_verified() {
        let db = setup_test_db().await;
        let token = AlertDestination::generate_verification_token();
        let expires_at = Utc::now() + chrono::Duration::hours(24);
        let config = serde_json::json!({"target": "address", "address": "ext@example.com"});

        AlertDestination::create_with_verification(
            &db,
            &test_org_id(),
            "Already Done",
            DestinationType::Email,
            &config,
            &test_user_id(),
            false,
            Some(&token),
            Some(expires_at),
        )
        .await
        .unwrap();

        // Verify once
        AlertDestination::verify_by_token(&db, &token)
            .await
            .unwrap();

        // Token is now cleared, so a second attempt with the same token should be NotFound
        let result = AlertDestination::verify_by_token(&db, &token).await;
        assert!(matches!(result, Err(AlertError::NotFound)));
    }

    #[tokio::test]
    async fn test_refresh_verification_token() {
        let db = setup_test_db().await;
        let token1 = AlertDestination::generate_verification_token();
        let expires_at1 = Utc::now() + chrono::Duration::hours(24);
        let config = serde_json::json!({"target": "address", "address": "ext@example.com"});

        let dest = AlertDestination::create_with_verification(
            &db,
            &test_org_id(),
            "Resend Test",
            DestinationType::Email,
            &config,
            &test_user_id(),
            false,
            Some(&token1),
            Some(expires_at1),
        )
        .await
        .unwrap();

        // Refresh token
        let token2 = AlertDestination::generate_verification_token();
        let expires_at2 = Utc::now() + chrono::Duration::hours(24);
        AlertDestination::refresh_verification_token(
            &db,
            &dest.alert_destination_id,
            &token2,
            expires_at2,
        )
        .await
        .unwrap();

        let refreshed = AlertDestination::get_by_id(&db, &dest.alert_destination_id)
            .await
            .unwrap();
        assert!(!refreshed.verified, "should still be unverified");
        assert_eq!(refreshed.verification_token, Some(token2.clone()));
        assert_eq!(
            refreshed.verification_attempts, 1,
            "attempts should be incremented"
        );

        // Old token should no longer work
        let result = AlertDestination::verify_by_token(&db, &token1).await;
        assert!(matches!(result, Err(AlertError::NotFound)));

        // New token should work
        let verified = AlertDestination::verify_by_token(&db, &token2)
            .await
            .unwrap();
        assert!(verified.verified);
    }

    #[tokio::test]
    async fn test_reset_verification() {
        let db = setup_test_db().await;
        let token1 = AlertDestination::generate_verification_token();
        let expires_at1 = Utc::now() + chrono::Duration::hours(24);
        let config = serde_json::json!({"target": "address", "address": "old@example.com"});

        let dest = AlertDestination::create_with_verification(
            &db,
            &test_org_id(),
            "Reset Test",
            DestinationType::Email,
            &config,
            &test_user_id(),
            false,
            Some(&token1),
            Some(expires_at1),
        )
        .await
        .unwrap();

        // First verify it
        AlertDestination::verify_by_token(&db, &token1)
            .await
            .unwrap();
        let verified = AlertDestination::get_by_id(&db, &dest.alert_destination_id)
            .await
            .unwrap();
        assert!(verified.verified);

        // Now reset (simulates email address change)
        let token2 = AlertDestination::generate_verification_token();
        let expires_at2 = Utc::now() + chrono::Duration::hours(24);
        AlertDestination::reset_verification(&db, &dest.alert_destination_id, &token2, expires_at2)
            .await
            .unwrap();

        let reset = AlertDestination::get_by_id(&db, &dest.alert_destination_id)
            .await
            .unwrap();
        assert!(!reset.verified, "should be unverified after reset");
        assert_eq!(reset.verification_token, Some(token2.clone()));
        assert_eq!(
            reset.verification_attempts, 0,
            "attempts should be reset to 0"
        );

        // New token should work to verify again
        let re_verified = AlertDestination::verify_by_token(&db, &token2)
            .await
            .unwrap();
        assert!(re_verified.verified);
    }

    #[tokio::test]
    async fn test_generate_verification_token_format() {
        let token = AlertDestination::generate_verification_token();
        assert_eq!(token.len(), 64, "token should be 64 chars");
        assert!(
            token.chars().all(|c| c.is_ascii_alphanumeric()),
            "token should be alphanumeric"
        );

        // Tokens should be unique
        let token2 = AlertDestination::generate_verification_token();
        assert_ne!(token, token2);
    }
}
