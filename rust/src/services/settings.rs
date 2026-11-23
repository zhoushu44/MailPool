//! 全局设置服务：provider 配置、API Key、计费/保活/健康/刷新配置。
//!
//! 存储：`settings` 表键值对，键名与 Python 版**逐字对齐**
//! （`provider_config.<id>` / `api_keys` / `billing.<key>` / `heartbeat_enabled` …），
//! 保证 Rust 版读写的是 Python 版留下的同一批数据。

use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};
use crate::util::now_naive;

// ────────────────────────────────────────────────────────────────
//  基础键值
// ────────────────────────────────────────────────────────────────

pub async fn get_value(pool: &SqlitePool, key: &str, default: &str) -> AppResult<String> {
    let row: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.unwrap_or_else(|| default.to_string()))
}

pub async fn set_value(pool: &SqlitePool, key: &str, value: &str) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO settings (key, value, updated_at) VALUES (?, ?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(key)
    .bind(value)
    .bind(now_naive())
    .execute(pool)
    .await?;
    Ok(())
}

// ────────────────────────────────────────────────────────────────
//  Provider 全局配置
// ────────────────────────────────────────────────────────────────

pub async fn get_provider_config(
    pool: &SqlitePool,
    provider_id: &str,
) -> AppResult<serde_json::Map<String, serde_json::Value>> {
    let raw = get_value(pool, &format!("provider_config.{provider_id}"), "{}").await?;
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(serde_json::Value::Object(m)) => Ok(m),
        _ => Ok(serde_json::Map::new()),
    }
}

pub async fn save_provider_config(
    pool: &SqlitePool,
    provider_id: &str,
    config: &serde_json::Map<String, serde_json::Value>,
) -> AppResult<()> {
    let raw = serde_json::to_string(config)?;
    set_value(pool, &format!("provider_config.{provider_id}"), &raw).await
}

// ────────────────────────────────────────────────────────────────
//  老式匿名 API Key（settings.api_keys，JSON 数组）
// ────────────────────────────────────────────────────────────────

pub async fn get_api_keys(pool: &SqlitePool) -> AppResult<Vec<String>> {
    let raw = get_value(pool, "api_keys", "[]").await?;
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(serde_json::Value::Array(a)) => Ok(a
            .into_iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect()),
        _ => Ok(Vec::new()),
    }
}

pub async fn add_api_key(pool: &SqlitePool, key: &str) -> AppResult<Vec<String>> {
    let mut keys = get_api_keys(pool).await?;
    if !keys.iter().any(|k| k == key) {
        keys.push(key.to_string());
        let raw = serde_json::to_string(&keys)?;
        set_value(pool, "api_keys", &raw).await?;
    }
    Ok(keys)
}

pub async fn remove_api_key(pool: &SqlitePool, key: &str) -> AppResult<Vec<String>> {
    let keys: Vec<String> = get_api_keys(pool)
        .await?
        .into_iter()
        .filter(|k| k != key)
        .collect();
    let raw = serde_json::to_string(&keys)?;
    set_value(pool, "api_keys", &raw).await?;
    Ok(keys)
}

// ────────────────────────────────────────────────────────────────
//  绑定用户的 API Key（计费版，存 api_keys 表）
// ────────────────────────────────────────────────────────────────

/// 给用户签发一个绑定身份的 API Key。
///
/// 只有绑了 user_id 的 key 才能计费；`settings.api_keys` 里的老 key 是匿名。
pub async fn create_user_api_key(
    pool: &SqlitePool,
    user_id: i64,
    name: &str,
) -> AppResult<String> {
    let key = crate::util::generate_api_key();
    sqlx::query(
        "INSERT INTO api_keys (user_id, key, name, enabled, created_at) \
         VALUES (?, ?, ?, 1, ?)",
    )
    .bind(user_id)
    .bind(&key)
    .bind(crate::util::truncate_chars(name, 64))
    .bind(now_naive())
    .execute(pool)
    .await?;
    Ok(key)
}

/// 列出用户 API Key（`user_id` 为 None 时列全部）。
pub async fn list_user_api_keys(
    pool: &SqlitePool,
    user_id: Option<i64>,
) -> AppResult<Vec<crate::models::ApiKey>> {
    let rows = match user_id {
        Some(uid) => {
            sqlx::query_as::<_, crate::models::ApiKey>(
                "SELECT * FROM api_keys WHERE user_id = ? ORDER BY id ASC",
            )
            .bind(uid)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as::<_, crate::models::ApiKey>("SELECT * FROM api_keys ORDER BY id ASC")
                .fetch_all(pool)
                .await?
        }
    };
    Ok(rows)
}

/// 删除（吊销）一个 API Key。
pub async fn delete_user_api_key(pool: &SqlitePool, key_id: i64) -> AppResult<bool> {
    let res = sqlx::query("DELETE FROM api_keys WHERE id = ?")
        .bind(key_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// 按 key 值吊销。
pub async fn revoke_api_key_by_value(pool: &SqlitePool, key: &str) -> AppResult<bool> {
    let res = sqlx::query("DELETE FROM api_keys WHERE key = ?")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

// ────────────────────────────────────────────────────────────────
//  计费配置
// ────────────────────────────────────────────────────────────────

/// 计费配置（默认值对齐 Python `BILLING_DEFAULTS`）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct BillingConfig {
    pub enabled: String,
    pub default_price: String,
    pub min_topup: String,
    pub max_topup: String,
    pub hold_timeout: String,
    pub award_percent: String,
    pub epay_url: String,
    pub epay_pid: String,
    pub epay_key: String,
    /// 展示哪些渠道（逗号分隔，默认 alipay）
    pub epay_types: String,
    pub site_url: String,
    pub card_default_amount: String,
}

impl Default for BillingConfig {
    fn default() -> Self {
        Self {
            enabled: "false".into(),
            default_price: "0.018".into(),
            min_topup: "1.00".into(),
            max_topup: "1000.00".into(),
            hold_timeout: "1200".into(),
            award_percent: "0".into(),
            epay_url: String::new(),
            epay_pid: String::new(),
            epay_key: String::new(),
    epay_types: "alipay".to_string(),
            site_url: String::new(),
            card_default_amount: "10.00".into(),
        }
    }
}

impl BillingConfig {
    pub const KEYS: &'static [&'static str] = &[
        "enabled",
        "default_price",
        "min_topup",
        "max_topup",
        "hold_timeout",
        "award_percent",
        "epay_url",
        "epay_pid",
        "epay_key",
        "site_url",
        "card_default_amount",
    ];

    pub fn is_enabled(&self) -> bool {
        matches!(
            self.enabled.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    }

    pub fn hold_timeout_secs(&self) -> i64 {
        self.hold_timeout.parse().unwrap_or(1200)
    }
}

pub async fn get_billing_config(pool: &SqlitePool) -> AppResult<BillingConfig> {
    // 一次查全部键，避免 Python 版每个键一次查询（55 项目 × 11 键 = 605 次）
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT key, value FROM settings WHERE key LIKE 'billing.%'")
            .fetch_all(pool)
            .await?;
    let map: std::collections::HashMap<&str, &str> = rows
        .iter()
        .filter_map(|(k, v)| k.strip_prefix("billing.").map(|s| (s, v.as_str())))
        .collect();

    let d = BillingConfig::default();
    let g = |k: &str, default: &str| {
        map.get(k)
            .map(|s| s.to_string())
            .unwrap_or_else(|| default.to_string())
    };

    Ok(BillingConfig {
        enabled: g("enabled", &d.enabled),
        default_price: g("default_price", &d.default_price),
        min_topup: g("min_topup", &d.min_topup),
        max_topup: g("max_topup", &d.max_topup),
        hold_timeout: g("hold_timeout", &d.hold_timeout),
        award_percent: g("award_percent", &d.award_percent),
        epay_url: g("epay_url", &d.epay_url),
        epay_pid: g("epay_pid", &d.epay_pid),
        epay_key: g("epay_key", &d.epay_key),
        epay_types: g("epay_types", &d.epay_types),
        site_url: g("site_url", &d.site_url),
        card_default_amount: g("card_default_amount", &d.card_default_amount),
    })
}

pub async fn save_billing_config(
    pool: &SqlitePool,
    patch: &serde_json::Value,
) -> AppResult<BillingConfig> {
    if let Some(obj) = patch.as_object() {
        for key in BillingConfig::KEYS {
            if let Some(v) = obj.get(*key) {
                let s = match v {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Null => continue,
                    other => other.to_string(),
                };
                set_value(pool, &format!("billing.{key}"), &s).await?;
            }
        }
    }
    get_billing_config(pool).await
}

// ────────────────────────────────────────────────────────────────
//  心跳保活 / 健康扫描 / 定时刷新 配置
// ────────────────────────────────────────────────────────────────

/// 心跳保活配置。
#[derive(Debug, Clone, serde::Serialize)]
pub struct HeartbeatConfig {
    pub heartbeat_enabled: String,
    pub heartbeat_mode: String,
    pub heartbeat_hour: String,
    pub heartbeat_minute: String,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            heartbeat_enabled: "false".into(),
            heartbeat_mode: "silent".into(),
            heartbeat_hour: "9".into(),
            heartbeat_minute: "0".into(),
        }
    }
}

pub async fn get_heartbeat_config(pool: &SqlitePool) -> AppResult<HeartbeatConfig> {
    let d = HeartbeatConfig::default();
    Ok(HeartbeatConfig {
        heartbeat_enabled: get_value(pool, "heartbeat_enabled", &d.heartbeat_enabled).await?,
        heartbeat_mode: get_value(pool, "heartbeat_mode", &d.heartbeat_mode).await?,
        heartbeat_hour: get_value(pool, "heartbeat_hour", &d.heartbeat_hour).await?,
        heartbeat_minute: get_value(pool, "heartbeat_minute", &d.heartbeat_minute).await?,
    })
}

pub async fn save_heartbeat_config(
    pool: &SqlitePool,
    patch: &serde_json::Value,
) -> AppResult<HeartbeatConfig> {
    for key in [
        "heartbeat_enabled",
        "heartbeat_mode",
        "heartbeat_hour",
        "heartbeat_minute",
    ] {
        if let Some(v) = patch.get(key) {
            if v.is_null() {
                continue;
            }
            let s = v.as_str().map(|x| x.to_string()).unwrap_or(v.to_string());
            set_value(pool, key, &s).await?;
        }
    }
    get_heartbeat_config(pool).await
}

/// 健康扫描配置。
#[derive(Debug, Clone, serde::Serialize)]
pub struct HealthCheckConfig {
    pub health_check_enabled: String,
    pub health_check_hour: String,
    pub health_check_minute: String,
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            // 默认开启：微软会静默把号打成 service abuse mode（token 全废）
            health_check_enabled: "true".into(),
            health_check_hour: "8".into(),
            health_check_minute: "0".into(),
        }
    }
}

pub async fn get_health_check_config(pool: &SqlitePool) -> AppResult<HealthCheckConfig> {
    let d = HealthCheckConfig::default();
    Ok(HealthCheckConfig {
        health_check_enabled: get_value(pool, "health_check_enabled", &d.health_check_enabled)
            .await?,
        health_check_hour: get_value(pool, "health_check_hour", &d.health_check_hour).await?,
        health_check_minute: get_value(pool, "health_check_minute", &d.health_check_minute).await?,
    })
}

pub async fn save_health_check_config(
    pool: &SqlitePool,
    patch: &serde_json::Value,
) -> AppResult<HealthCheckConfig> {
    for key in [
        "health_check_enabled",
        "health_check_hour",
        "health_check_minute",
    ] {
        if let Some(v) = patch.get(key) {
            if v.is_null() {
                continue;
            }
            let s = v.as_str().map(|x| x.to_string()).unwrap_or(v.to_string());
            set_value(pool, key, &s).await?;
        }
    }
    get_health_check_config(pool).await
}

/// 定时全量刷新 Token 配置。
#[derive(Debug, Clone, serde::Serialize)]
pub struct RefreshConfig {
    pub refresh_enabled: String,
    pub refresh_hour: String,
    pub refresh_minute: String,
}

impl Default for RefreshConfig {
    fn default() -> Self {
        Self {
            refresh_enabled: "false".into(),
            refresh_hour: "3".into(),
            refresh_minute: "0".into(),
        }
    }
}

pub async fn get_refresh_config(pool: &SqlitePool) -> AppResult<RefreshConfig> {
    let d = RefreshConfig::default();
    Ok(RefreshConfig {
        refresh_enabled: get_value(pool, "refresh_enabled", &d.refresh_enabled).await?,
        refresh_hour: get_value(pool, "refresh_hour", &d.refresh_hour).await?,
        refresh_minute: get_value(pool, "refresh_minute", &d.refresh_minute).await?,
    })
}

pub async fn save_refresh_config(
    pool: &SqlitePool,
    patch: &serde_json::Value,
) -> AppResult<RefreshConfig> {
    for key in ["refresh_enabled", "refresh_hour", "refresh_minute"] {
        if let Some(v) = patch.get(key) {
            if v.is_null() {
                continue;
            }
            let s = v.as_str().map(|x| x.to_string()).unwrap_or(v.to_string());
            set_value(pool, key, &s).await?;
        }
    }
    get_refresh_config(pool).await
}

// ────────────────────────────────────────────────────────────────
//  上次运行摘要
// ────────────────────────────────────────────────────────────────

pub async fn set_last_run(
    pool: &SqlitePool,
    run_key: &str,
    summary_key: &str,
    summary: &serde_json::Value,
) -> AppResult<()> {
    set_value(pool, run_key, &crate::util::utc_now().to_rfc3339()).await?;
    set_value(pool, summary_key, &serde_json::to_string(summary)?).await
}

pub async fn get_last_run(
    pool: &SqlitePool,
    summary_key: &str,
) -> AppResult<serde_json::Value> {
    let raw = get_value(pool, summary_key, "{}").await?;
    Ok(serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn billing_defaults_match_python() {
        let d = BillingConfig::default();
        assert_eq!(d.default_price, "0.018");
        assert_eq!(d.min_topup, "1.00");
        assert_eq!(d.max_topup, "1000.00");
        assert_eq!(d.hold_timeout, "1200");
        assert_eq!(d.card_default_amount, "10.00");
        assert!(!d.is_enabled());
    }

    #[test]
    fn enabled_parsing() {
        for v in ["1", "true", "TRUE", "yes", "on"] {
            let c = BillingConfig {
                enabled: v.into(),
                ..Default::default()
            };
            assert!(c.is_enabled(), "{v} should be enabled");
        }
        for v in ["0", "false", "", "no"] {
            let c = BillingConfig {
                enabled: v.into(),
                ..Default::default()
            };
            assert!(!c.is_enabled(), "{v} should be disabled");
        }
    }

    #[test]
    fn health_check_defaults_on() {
        // 默认必须开启：关掉等于让死号一直躺在池子里发给客户
        assert_eq!(HealthCheckConfig::default().health_check_enabled, "true");
    }
}
