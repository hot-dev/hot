use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::FromRow;
use thiserror::Error;
use uuid::Uuid;

use super::DatabasePool;

type OrgNoteRow = (
    Uuid,
    Uuid,
    String,
    String,
    String,
    Option<String>,
    Option<Uuid>,
    DateTime<Utc>,
);

#[derive(Error, Debug)]
pub enum OrgNoteError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// Internal notes attached to an organization.
///
/// Append-only audit trail for admin and system use. Notes are never
/// updated or deleted.
///
/// ## Categories
/// - `billing` — budget cap events, meter failures, plan exceptions
/// - `features` — manual feature overrides with reason
/// - `support` — abuse flags, customer interactions
/// - `security` — key revocations, suspicious activity
/// - `internal` — general admin notes
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct OrgNote {
    pub note_id: Uuid,
    pub org_id: Uuid,
    pub category: String,
    pub note_type: String,
    pub message: String,
    #[sqlx(default)]
    pub metadata: Option<JsonValue>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

impl OrgNote {
    /// Create a system-generated note (no created_by user).
    pub async fn create_system(
        db: &DatabasePool,
        org_id: &Uuid,
        category: &str,
        note_type: &str,
        message: &str,
        metadata: Option<&JsonValue>,
    ) -> Result<Uuid, OrgNoteError> {
        Self::create(db, org_id, category, note_type, message, metadata, None).await
    }

    /// Create a note with an optional user attribution.
    pub async fn create(
        db: &DatabasePool,
        org_id: &Uuid,
        category: &str,
        note_type: &str,
        message: &str,
        metadata: Option<&JsonValue>,
        created_by: Option<&Uuid>,
    ) -> Result<Uuid, OrgNoteError> {
        let note_id = Uuid::now_v7();

        match db {
            DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO org_note
                     (note_id, org_id, category, note_type, message, metadata, created_by)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(note_id)
                .bind(org_id)
                .bind(category)
                .bind(note_type)
                .bind(message)
                .bind(metadata)
                .bind(created_by)
                .execute(pool)
                .await?;
            }
            DatabasePool::Sqlite(pool) => {
                let metadata_str = metadata.map(|m| serde_json::to_string(m).unwrap_or_default());
                sqlx::query(
                    "INSERT INTO org_note
                     (note_id, org_id, category, note_type, message, metadata, created_by)
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(note_id)
                .bind(org_id)
                .bind(category)
                .bind(note_type)
                .bind(message)
                .bind(metadata_str)
                .bind(created_by)
                .execute(pool)
                .await?;
            }
        }

        Ok(note_id)
    }

    /// List notes for an org, newest first.
    pub async fn list_by_org(
        db: &DatabasePool,
        org_id: &Uuid,
        limit: i64,
    ) -> Result<Vec<Self>, OrgNoteError> {
        match db {
            DatabasePool::Postgres(pool) => {
                let notes = sqlx::query_as::<_, Self>(
                    "SELECT note_id, org_id, category, note_type, message, metadata, created_by, created_at
                     FROM org_note
                     WHERE org_id = $1
                     ORDER BY created_at DESC
                     LIMIT $2",
                )
                .bind(org_id)
                .bind(limit)
                .fetch_all(pool)
                .await?;
                Ok(notes)
            }
            DatabasePool::Sqlite(pool) => {
                let rows: Vec<OrgNoteRow> =
                    sqlx::query_as(
                        "SELECT note_id, org_id, category, note_type, message, metadata, created_by, created_at
                         FROM org_note
                         WHERE org_id = ?
                         ORDER BY created_at DESC
                         LIMIT ?",
                    )
                    .bind(org_id)
                    .bind(limit)
                    .fetch_all(pool)
                    .await?;

                Ok(rows
                    .into_iter()
                    .map(
                        |(
                            note_id,
                            org_id,
                            category,
                            note_type,
                            message,
                            metadata_str,
                            created_by,
                            created_at,
                        )| {
                            OrgNote {
                                note_id,
                                org_id,
                                category,
                                note_type,
                                message,
                                metadata: metadata_str.and_then(|s| serde_json::from_str(&s).ok()),
                                created_by,
                                created_at,
                            }
                        },
                    )
                    .collect())
            }
        }
    }

    /// List notes for an org filtered by category, newest first.
    pub async fn list_by_category(
        db: &DatabasePool,
        org_id: &Uuid,
        category: &str,
        limit: i64,
    ) -> Result<Vec<Self>, OrgNoteError> {
        match db {
            DatabasePool::Postgres(pool) => {
                let notes = sqlx::query_as::<_, Self>(
                    "SELECT note_id, org_id, category, note_type, message, metadata, created_by, created_at
                     FROM org_note
                     WHERE org_id = $1 AND category = $2
                     ORDER BY created_at DESC
                     LIMIT $3",
                )
                .bind(org_id)
                .bind(category)
                .bind(limit)
                .fetch_all(pool)
                .await?;
                Ok(notes)
            }
            DatabasePool::Sqlite(pool) => {
                let rows: Vec<OrgNoteRow> =
                    sqlx::query_as(
                        "SELECT note_id, org_id, category, note_type, message, metadata, created_by, created_at
                         FROM org_note
                         WHERE org_id = ? AND category = ?
                         ORDER BY created_at DESC
                         LIMIT ?",
                    )
                    .bind(org_id)
                    .bind(category)
                    .bind(limit)
                    .fetch_all(pool)
                    .await?;

                Ok(rows
                    .into_iter()
                    .map(
                        |(
                            note_id,
                            org_id,
                            category,
                            note_type,
                            message,
                            metadata_str,
                            created_by,
                            created_at,
                        )| {
                            OrgNote {
                                note_id,
                                org_id,
                                category,
                                note_type,
                                message,
                                metadata: metadata_str.and_then(|s| serde_json::from_str(&s).ok()),
                                created_by,
                                created_at,
                            }
                        },
                    )
                    .collect())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_org_note_struct_fields() {
        let note = OrgNote {
            note_id: Uuid::now_v7(),
            org_id: Uuid::now_v7(),
            category: "billing".to_string(),
            note_type: "budget_cap_applied".to_string(),
            message: "CUS budget cap applied: 12,450 raw capped to 10,000".to_string(),
            metadata: Some(serde_json::json!({
                "raw_cus": 12450,
                "capped_cus": 10000,
                "budget": 15000,
                "included": 5000
            })),
            created_by: None,
            created_at: Utc::now(),
        };

        assert_eq!(note.category, "billing");
        assert_eq!(note.note_type, "budget_cap_applied");
        assert!(note.created_by.is_none());
        assert!(note.metadata.is_some());
    }

    #[test]
    fn test_org_note_with_user() {
        let user_id = Uuid::now_v7();
        let note = OrgNote {
            note_id: Uuid::now_v7(),
            org_id: Uuid::now_v7(),
            category: "features".to_string(),
            note_type: "feature_override".to_string(),
            message: "Set compute_units_per_month to 1000000".to_string(),
            metadata: Some(serde_json::json!({
                "key": "compute_units_per_month",
                "old_value": 500000,
                "new_value": 1000000,
                "reason": "Enterprise pilot"
            })),
            created_by: Some(user_id),
            created_at: Utc::now(),
        };

        assert_eq!(note.created_by, Some(user_id));
    }
}
