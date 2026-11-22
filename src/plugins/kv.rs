//! The plugins' key-value store (`PluginKeyValueStore`): values per plugin and key, optionally expiring. Old
//! keys were stored as base64(sha256(key)); reads fall back to that form and writes remove it.

use base64::Engine as _;
use sha2::Digest as _;

use crate::app::{ApiResult, App, AppError};

/// The old, hashed form of a key.
fn hashed(key: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(key.as_bytes()))
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn check(plugin_id: &str, key: &str) -> ApiResult<()> {
    if plugin_id.is_empty() || plugin_id.chars().count() > 190 {
        return Err(AppError::bad_request("model.plugin_key_value.is_valid.plugin_id.app_error", "Invalid plugin id"));
    }
    if key.is_empty() || key.chars().count() > 150 {
        return Err(AppError::bad_request("model.plugin_key_value.is_valid.key.app_error", "Invalid key"));
    }
    Ok(())
}

async fn get_exact(app: &App, plugin_id: &str, key: &str) -> ApiResult<Option<Vec<u8>>> {
    Ok(sqlx::query_scalar(
        "SELECT PValue FROM PluginKeyValueStore WHERE PluginId = $1 AND PKey = $2 AND (ExpireAt = 0 OR ExpireAt > $3)",
    )
    .bind(plugin_id)
    .bind(key)
    .bind(now())
    .fetch_optional(&app.db)
    .await?
    .flatten())
}

/// The value, or the one stored under the key's old hashed form.
pub async fn get(app: &App, plugin_id: &str, key: &str) -> ApiResult<Option<Vec<u8>>> {
    if let Some(v) = get_exact(app, plugin_id, key).await? {
        return Ok(Some(v));
    }
    get_exact(app, plugin_id, &hashed(key)).await
}

async fn delete_exact(app: &App, plugin_id: &str, key: &str) -> ApiResult<()> {
    sqlx::query("DELETE FROM PluginKeyValueStore WHERE PluginId = $1 AND PKey = $2")
        .bind(plugin_id)
        .bind(key)
        .execute(&app.db)
        .await?;
    Ok(())
}

pub async fn delete(app: &App, plugin_id: &str, key: &str) -> ApiResult<()> {
    delete_exact(app, plugin_id, &hashed(key)).await?;
    delete_exact(app, plugin_id, key).await
}

pub async fn delete_all(app: &App, plugin_id: &str) -> ApiResult<()> {
    sqlx::query("DELETE FROM PluginKeyValueStore WHERE PluginId = $1").bind(plugin_id).execute(&app.db).await?;
    Ok(())
}

/// Unexpired keys in order.
pub async fn list(app: &App, plugin_id: &str, offset: i64, limit: i64) -> ApiResult<Vec<String>> {
    let limit = if limit <= 0 { 10 } else { limit };
    Ok(sqlx::query_scalar(
        "SELECT PKey FROM PluginKeyValueStore WHERE PluginId = $1 AND (ExpireAt = 0 OR ExpireAt > $2) \
         ORDER BY PKey LIMIT $3 OFFSET $4",
    )
    .bind(plugin_id)
    .bind(now())
    .bind(limit)
    .bind(offset.max(0))
    .fetch_all(&app.db)
    .await?)
}

#[derive(Default)]
pub struct SetOptions {
    pub atomic: bool,
    pub old_value: Option<Vec<u8>>,
    pub expire_in_seconds: i64,
}

/// Sets a value: none deletes it; atomic sets only over `old_value` (none: only where the key is
/// absent). Whether the value was set.
pub async fn set(app: &App, plugin_id: &str, key: &str, value: Option<Vec<u8>>, o: SetOptions) -> ApiResult<bool> {
    if !o.atomic && o.old_value.is_some() {
        return Err(AppError::bad_request(
            "model.plugin_kvset_options.is_valid.old_value.app_error",
            "Invalid old value, it shouldn't be set when the operation is not atomic.",
        ));
    }
    check(plugin_id, key)?;
    let expire_at =
        if o.expire_in_seconds > 0 { now().saturating_add(o.expire_in_seconds.saturating_mul(1000)) } else { 0 };
    let set = match (o.atomic, value) {
        (false, None) => {
            delete_exact(app, plugin_id, key).await?;
            true
        }
        (false, Some(v)) => {
            sqlx::query(
                "INSERT INTO PluginKeyValueStore (PluginId, PKey, PValue, ExpireAt) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (PluginId, PKey) DO UPDATE SET PValue = EXCLUDED.PValue, ExpireAt = EXCLUDED.ExpireAt",
            )
            .bind(plugin_id)
            .bind(key)
            .bind(v)
            .bind(expire_at)
            .execute(&app.db)
            .await?;
            true
        }
        (true, None) => compare_and_delete(app, plugin_id, key, o.old_value.as_deref()).await?,
        (true, Some(v)) => match &o.old_value {
            None => {
                sqlx::query(
                    "DELETE FROM PluginKeyValueStore WHERE PluginId = $1 AND PKey = $2 AND ExpireAt != 0 \
                     AND ExpireAt < $3",
                )
                .bind(plugin_id)
                .bind(key)
                .bind(now())
                .execute(&app.db)
                .await?;
                sqlx::query(
                    "INSERT INTO PluginKeyValueStore (PluginId, PKey, PValue, ExpireAt) VALUES ($1, $2, $3, $4) \
                     ON CONFLICT DO NOTHING",
                )
                .bind(plugin_id)
                .bind(key)
                .bind(v)
                .bind(expire_at)
                .execute(&app.db)
                .await?
                .rows_affected()
                    > 0
            }
            Some(old) => {
                sqlx::query(
                    "UPDATE PluginKeyValueStore SET PValue = $1, ExpireAt = $2 WHERE PluginId = $3 AND PKey = $4 \
                     AND PValue = $5 AND (ExpireAt = 0 OR ExpireAt > $6)",
                )
                .bind(v)
                .bind(expire_at)
                .bind(plugin_id)
                .bind(key)
                .bind(old)
                .bind(now())
                .execute(&app.db)
                .await?
                .rows_affected()
                    > 0
            }
        },
    };
    delete_exact(app, plugin_id, &hashed(key)).await?;
    Ok(set)
}

/// Deletes the key only while it holds `old` (nothing is ever stored as nil).
pub async fn compare_and_delete(app: &App, plugin_id: &str, key: &str, old: Option<&[u8]>) -> ApiResult<bool> {
    let Some(old) = old else { return Ok(false) };
    Ok(sqlx::query(
        "DELETE FROM PluginKeyValueStore WHERE PluginId = $1 AND PKey = $2 AND PValue = $3 \
         AND (ExpireAt = 0 OR ExpireAt > $4)",
    )
    .bind(plugin_id)
    .bind(key)
    .bind(old)
    .bind(now())
    .execute(&app.db)
    .await?
    .rows_affected()
        > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_keys_are_hashed() {
        assert_eq!(hashed("mmi_botid"), "+1HRWesrArLbP5DeXazZihN1i6AoAnhuDn9w4RQpJxQ=");
        assert!(check("jira", "").is_err());
        assert!(check("jira", &"ж".repeat(150)).is_ok());
        assert!(check("jira", &"ж".repeat(151)).is_err());
    }
}
