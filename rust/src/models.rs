//! 领域模型：与 SQLite 行一一对应的结构体。
//!
//! 字段顺序、可空性、默认值全部对齐 Python `app/models.py`。
//! 时间列统一 `Option<NaiveDateTime>`（数据库存 naive UTC 文本）。

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::money::Money;

// ────────────────────────────────────────────────────────────────
//  状态常量（对齐 Python models.py 顶部的字符串常量）
// ────────────────────────────────────────────────────────────────

// 号池状态
pub const POOL_AVAILABLE: &str = "available";
pub const POOL_USING: &str = "using";
pub const POOL_RETIRED: &str = "retired";

// 健康状态
pub const HEALTH_UNKNOWN: &str = "unknown";
pub const HEALTH_HEALTHY: &str = "healthy";
pub const HEALTH_UNHEALTHY: &str = "unhealthy";
pub const HEALTH_DEAD: &str = "dead";

/// 可发放的健康状态（对齐 Python `CLAIMABLE_HEALTH_STATUSES`）
pub const CLAIMABLE_HEALTH_STATUSES: &[&str] = &[HEALTH_HEALTHY, HEALTH_UNKNOWN];

// 计费订单状态
pub const BILLING_PENDING: &str = "pending";
pub const BILLING_CHARGED: &str = "charged";
pub const BILLING_FREE: &str = "free";

// 余额流水类型
pub const LOG_RECHARGE: &str = "recharge"; // 扫码充值
pub const LOG_REDEEM: &str = "redeem"; // 充值卡
pub const LOG_AWARD: &str = "award"; // 充值赠送
pub const LOG_ADJUST: &str = "adjust"; // 管理员调整
pub const LOG_HOLD: &str = "hold"; // 取号冻结
pub const LOG_CHARGE: &str = "charge"; // 收码实扣
pub const LOG_RELEASE: &str = "release"; // 释放解冻

// 充值订单状态
pub const TOPUP_PENDING: &str = "pending";
pub const TOPUP_SUCCESS: &str = "success";
pub const TOPUP_FAILED: &str = "failed";

// 用户角色
pub const ROLE_ADMIN: i64 = 1;
pub const ROLE_USER: i64 = 2;

// ────────────────────────────────────────────────────────────────
//  实体
// ────────────────────────────────────────────────────────────────

/// 项目 = 一个 project_code + 指定 provider 及其参数。
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Project {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub provider: String,
    pub config_json: String,
    pub enabled: i64,
    pub created_at: NaiveDateTime,
}

impl Project {
    /// 解析 `config_json`；解析失败返回空对象（对齐 Python 的 try/except）。
    pub fn config(&self) -> serde_json::Value {
        serde_json::from_str(&self.config_json).unwrap_or_else(|_| serde_json::json!({}))
    }

    /// 取 config 里的字符串字段。
    pub fn cfg_str(&self, key: &str) -> String {
        let v = self.config();
        v.get(key)
            .and_then(|x| {
                if x.is_string() {
                    x.as_str().map(|s| s.to_string())
                } else if x.is_null() {
                    None
                } else {
                    Some(x.to_string())
                }
            })
            .unwrap_or_default()
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled != 0
    }
}

/// 号池账号。
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct EmailAccount {
    pub id: i64,
    pub email: String,
    pub provider: String,
    pub account_json: String,
    #[sqlx(default)]
    pub status: String,
    #[sqlx(default)]
    pub remark: String,
    pub last_check_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
    #[sqlx(default)]
    pub check_result: String,
    #[sqlx(default)]
    pub fail_count: i64,
    #[sqlx(default)]
    pub pool_status: String,
    #[sqlx(default)]
    pub health_status: String,
    #[sqlx(default)]
    pub health_reason: String,
    pub claimed_at: Option<NaiveDateTime>,
    #[sqlx(default)]
    pub tags: String,
    #[sqlx(default)]
    pub proxy: String,
}

impl EmailAccount {
    /// 解析账号凭证 JSON。
    pub fn credentials(&self) -> serde_json::Value {
        serde_json::from_str(&self.account_json).unwrap_or_else(|_| serde_json::json!({}))
    }

    pub fn is_claimable(&self) -> bool {
        self.pool_status == POOL_AVAILABLE
            && CLAIMABLE_HEALTH_STATUSES.contains(&self.health_status.as_str())
    }

    /// 页面展示用的单一状态口径。
    ///
    /// **为什么不用 `status` 列**：那是 v1 的遗留列，迁移后与新状态语义脱节
    /// （`pool_status='using'` 时 `status` 可能还是 `'available'`），
    /// pool.html 直接读它会导致「明明被占用却显示可用」。
    ///
    /// 派生优先级（互斥，一个号只能落一格）：
    /// 1. `pool_status=using`     → `using`（正在被某个项目占用，最重要）
    /// 2. `pool_status=retired`   → `retired`（已退役，不再发放）
    /// 3. `health_status=healthy` → `healthy`（可用且已验证过）
    /// 4. `health_status=unhealthy` → `unhealthy`（可用但检测失败过）
    /// 5. `health_status=dead`    → `dead`（失效）
    /// 6. 其余                     → `available`（可用但未检测）
    pub fn display_status(&self) -> &'static str {
        match (self.pool_status.as_str(), self.health_status.as_str()) {
            (POOL_USING, _) => POOL_USING,
            (POOL_RETIRED, _) => POOL_RETIRED,
            (_, HEALTH_DEAD) => HEALTH_DEAD,
            (_, HEALTH_HEALTHY) => HEALTH_HEALTHY,
            (_, HEALTH_UNHEALTHY) => HEALTH_UNHEALTHY,
            _ => POOL_AVAILABLE,
        }
    }
}

/// 收件记录。
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct MailRecord {
    pub id: i64,
    pub email: String,
    #[sqlx(default)]
    pub project_code: String,
    #[sqlx(default)]
    pub message_id: String,
    #[sqlx(default)]
    pub provider: String,
    #[sqlx(default)]
    pub folder: String,
    #[sqlx(default)]
    pub sender: String,
    #[sqlx(default)]
    pub subject: String,
    #[sqlx(default)]
    pub body_preview: String,
    #[sqlx(default)]
    pub body: String,
    #[sqlx(default)]
    pub code: String,
    #[sqlx(default)]
    pub code_source: String,
    pub received_at: NaiveDateTime,
}

/// 邮箱使用记录（按项目防复用）。
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct EmailUsage {
    pub id: i64,
    pub email: String,
    pub project_code: String,
    pub first_used_at: NaiveDateTime,
}

/// 取号瞬间的收件箱基线（防"旧验证码"误判）。
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct MailBaseline {
    pub id: i64,
    pub email: String,
    #[sqlx(default)]
    pub project_code: String,
    pub baseline_at: NaiveDateTime,
    #[sqlx(default)]
    pub message_ids: String,
    pub latest_received_at: Option<NaiveDateTime>,
    #[sqlx(default)]
    pub captured: i64,
}

impl MailBaseline {
    /// 解析基线里的 message id 列表（逗号分隔）。
    pub fn message_id_set(&self) -> std::collections::HashSet<String> {
        self.message_ids
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }
}

/// 平台用户（客户）。
///
/// ## `role` 字段为什么要宽松解码
///
/// 生产库的真实情况（2026-11-22 实测）：
/// ```sql
/// CREATE TABLE users (... role INTEGER NOT NULL ...)   -- 声明是 INTEGER
/// SELECT role, typeof(role) FROM users;  -->  'admin', 'text'   -- 实际是 TEXT
/// ```
///
/// SQLite 是**动态类型**：列声明只是 affinity，写什么就存什么。历史上有过
/// `role='admin'` 这样的文本写入，于是同一列里既有整数也有字符串。
///
/// Python 用 ORM 读时宽容处理（`"admin"` 当字符串用），Rust 的 sqlx 严格
/// 解码会直接报 `mismatched types; Rust type i64 is not compatible with
/// SQL TEXT` —— 整个用户列表接口 500。
///
/// 这里改成先按 `Value` 读再归一化：数字直接用，文本按名字映射。
/// 这样新旧数据都能读，也不需要动生产数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    #[serde(default)]
    pub password_hash: String,
    #[serde(default)]
    pub role: i64,
    #[serde(default)]
    pub status: i64,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub balance: i64,
    #[serde(default)]
    pub frozen_balance: i64,
    #[serde(default)]
    pub total_recharged: i64,
    #[serde(default)]
    pub total_charged: i64,
    pub created_at: NaiveDateTime,
}

impl User {
    /// 从数据库行构造（`role` / `status` 宽松解析）。
    ///
    /// 不用 `#[derive(FromRow)]` 就是为了绕开 role 的类型漂移。
    pub fn from_row(row: &sqlx::sqlite::SqliteRow) -> Self {
        use sqlx::Row;

        fn as_i64_loose(row: &sqlx::sqlite::SqliteRow, col: &str, default: i64) -> i64 {
            // 先试整数
            if let Ok(v) = row.try_get::<i64, _>(col) {
                return v;
            }
            // 再试文本 → 按名字映射
            if let Ok(s) = row.try_get::<String, _>(col) {
                let low = s.trim().to_ascii_lowercase();
                return match low.as_str() {
                    "root" => 100,
                    "admin" => ROLE_ADMIN,
                    "user" | "common" | "" => ROLE_USER,
                    other => other.parse().unwrap_or(default),
                };
            }
            // 兜底：可能是 NULL
            default
        }

        Self {
            id: row.try_get("id").unwrap_or(0),
            username: row.try_get("username").unwrap_or_default(),
            password_hash: row.try_get("password_hash").unwrap_or_default(),
            role: as_i64_loose(row, "role", ROLE_USER),
            status: as_i64_loose(row, "status", 1),
            email: row.try_get("email").unwrap_or_default(),
            balance: as_i64_loose(row, "balance", 0),
            frozen_balance: as_i64_loose(row, "frozen_balance", 0),
            total_recharged: as_i64_loose(row, "total_recharged", 0),
            total_charged: as_i64_loose(row, "total_charged", 0),
            created_at: row
                .try_get("created_at")
                .unwrap_or_else(|_| chrono::Local::now().naive_local()),
        }
    }
}

impl User {
    pub fn is_active(&self) -> bool {
        self.status == 1
    }

    /// 可用余额 = 总余额 - 冻结。
    pub fn available_balance(&self) -> Money {
        Money::from_milli(self.balance - self.frozen_balance)
    }

    pub fn balance_money(&self) -> Money {
        Money::from_milli(self.balance)
    }
}

/// API Key（绑定用户）。
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ApiKey {
    pub id: i64,
    #[sqlx(default)]
    pub user_id: i64,
    pub key: String,
    #[sqlx(default)]
    pub name: String,
    #[sqlx(default)]
    pub enabled: i64,
    pub created_at: NaiveDateTime,
    pub last_used_at: Option<NaiveDateTime>,
}

/// 计费订单（三段式：冻结 → 实扣 / 解冻）。
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct EmailOrder {
    pub id: i64,
    pub user_id: i64,
    pub email: String,
    #[sqlx(default)]
    pub project_code: String,
    #[sqlx(default)]
    pub provider: String,
    #[sqlx(default)]
    pub price: i64,
    #[sqlx(default)]
    pub billing_status: String,
    #[sqlx(default)]
    pub hold_amount: i64,
    #[sqlx(default)]
    pub charged_amount: i64,
    pub created_at: NaiveDateTime,
    pub charged_at: Option<NaiveDateTime>,
    pub released_at: Option<NaiveDateTime>,
}

impl EmailOrder {
    pub fn price_money(&self) -> Money {
        Money::from_milli(self.price)
    }

    pub fn is_pending(&self) -> bool {
        self.billing_status == BILLING_PENDING
    }
}

/// 余额流水（唯一事实来源）。
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct BalanceLog {
    pub id: i64,
    pub user_id: i64,
    pub log_type: String,
    pub amount: i64,
    pub balance_before: i64,
    pub balance_after: i64,
    pub frozen_before: i64,
    pub frozen_after: i64,
    #[sqlx(default)]
    pub remark: String,
    pub created_at: NaiveDateTime,
}

/// 充值订单。
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct RechargeOrder {
    pub id: i64,
    pub user_id: i64,
    pub trade_no: String,
    #[sqlx(default)]
    pub amount: i64,
    #[sqlx(default)]
    pub money: i64,
    #[sqlx(default)]
    pub pay_type: String,
    #[sqlx(default)]
    pub pay_provider: String,
    #[sqlx(default)]
    pub status: String,
    #[sqlx(default)]
    pub client_ip: String,
    #[sqlx(default)]
    pub remark: String,
    pub created_at: NaiveDateTime,
    pub completed_at: Option<NaiveDateTime>,
}

/// 充值卡。
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct RedeemCard {
    pub id: i64,
    #[sqlx(default)]
    pub batch_id: String,
    pub card_key: String,
    #[sqlx(default)]
    pub amount: i64,
    #[sqlx(default)]
    pub status: i64,
    #[sqlx(default)]
    pub used_by: i64,
    pub used_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
}

/// Token 刷新日志。
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TokenRefreshLog {
    pub id: i64,
    #[sqlx(rename = "trigger", default)]
    pub trigger: String,
    #[sqlx(default)]
    pub provider: String,
    #[sqlx(default)]
    pub status: String,
    #[sqlx(default)]
    pub total: i64,
    #[sqlx(default)]
    pub success: i64,
    #[sqlx(default)]
    pub failed: i64,
    #[sqlx(default)]
    pub skipped: i64,
    #[sqlx(default)]
    pub duration: f64,
    #[sqlx(default)]
    pub detail: String,
    pub created_at: NaiveDateTime,
}

/// 全局设置键值。
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Setting {
    pub key: String,
    pub value: String,
    pub updated_at: NaiveDateTime,
}

/// 号池统计（页面展示用）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct PoolCounts {
    pub total: i64,
    pub available: i64,
    pub using: i64,
    pub dead: i64,
    pub retired: i64,
}

/// 健康汇总。
#[derive(Debug, Clone, Default, Serialize)]
pub struct HealthSummary {
    pub total: i64,
    pub healthy: i64,
    pub unhealthy: i64,
    pub unknown: i64,
    pub dead: i64,
    pub last_scan_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc(pool_status: &str, health_status: &str) -> EmailAccount {
        EmailAccount {
            id: 1,
            email: "a@b.test".into(),
            provider: "outlook".into(),
            account_json: "{}".into(),
            status: "available".into(), // 遗留列，故意设成误导值
            remark: String::new(),
            last_check_at: None,
            created_at: chrono::Local::now().naive_local(),
            check_result: String::new(),
            fail_count: 0,
            pool_status: pool_status.into(),
            health_status: health_status.into(),
            health_reason: String::new(),
            claimed_at: None,
            tags: String::new(),
            proxy: String::new(),
        }
    }

    #[test]
    fn display_status_prioritises_using() {
        // 遗留 status 列写着 available，但 pool_status=using → 必须显示占用
        assert_eq!(acc("using", "healthy").display_status(), "using");
        assert_eq!(acc("using", "unknown").display_status(), "using");
        assert_eq!(acc("using", "dead").display_status(), "using");
    }

    #[test]
    fn display_status_retired_beats_health() {
        assert_eq!(acc("retired", "healthy").display_status(), "retired");
        assert_eq!(acc("retired", "dead").display_status(), "retired");
    }

    #[test]
    fn display_status_health_fallbacks() {
        assert_eq!(acc("available", "healthy").display_status(), "healthy");
        assert_eq!(acc("available", "unhealthy").display_status(), "unhealthy");
        assert_eq!(acc("available", "dead").display_status(), "dead");
        assert_eq!(acc("available", "unknown").display_status(), "available");
    }

    #[test]
    fn claimable_requires_available_and_ok_health() {
        assert!(acc("available", "healthy").is_claimable());
        assert!(acc("available", "unknown").is_claimable());
        assert!(!acc("available", "unhealthy").is_claimable());
        assert!(!acc("available", "dead").is_claimable());
        assert!(!acc("using", "healthy").is_claimable());
        assert!(!acc("retired", "healthy").is_claimable());
    }
}
