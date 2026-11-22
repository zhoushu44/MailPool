//! 独占邮箱：买断一个邮箱，长期持有，收所有邮件。
//!
//! ## 与临时邮箱的区别
//!
//! | | 临时邮箱 | 独占邮箱 |
//! |---|---|---|
//! | 绑定维度 | `(email, project_code)` | `(email, user_id)` |
//! | 池状态 | `using`，超时自动回池 | `exclusive`，**永久占用** |
//! | 邮件过滤 | 只返回命中项目特征的 | **全部返回** |
//! | 释放 | 自动超时 | **手动释放** |
//! | 占用时长 | 2 分钟 | 不限 |
//!
//! 按 MailNest 的文档定义：
//! > 购买后，该邮箱仅属于你，不会分配给其他用户使用。
//! > 支持重复收件与长期持有。
//!
//! ## 为什么单独一张表
//!
//! 不复用 `email_usages`：那张表是「某项目用过某邮箱」的去重记录，
//! 每个项目一条，语义是**永久排除**。独占邮箱是「某人拥有某邮箱」，
//! 语义是**当前归属**，可以释放、可以转让。
//!
//! 混在一起会让「临时邮箱撞到独占邮箱」的判定变得极其难写。

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};
use crate::util::now_naive;

/// 独占邮箱的一条归属记录。
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ExclusiveEmail {
    pub id: i64,
    pub user_id: i64,
    pub email: String,
    pub provider: String,
    /// 购买时的单价（厘）
    pub price: i64,
    /// active / released
    pub status: String,
    pub created_at: chrono::NaiveDateTime,
    pub released_at: Option<chrono::NaiveDateTime>,
    /// 给用户看的备注（可选）
    #[sqlx(default)]
    pub note: String,
}

impl ExclusiveEmail {
    pub fn is_active(&self) -> bool {
        self.status == "active"
    }
}

/// 建表（幂等）。
pub async fn ensure_schema(pool: &SqlitePool) -> AppResult<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS exclusive_emails (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL,
            email VARCHAR(254) NOT NULL,
            provider VARCHAR(32) NOT NULL DEFAULT 'outlook',
            price BIGINT NOT NULL DEFAULT 0,
            status VARCHAR(16) NOT NULL DEFAULT 'active',
            created_at DATETIME NOT NULL,
            released_at DATETIME,
            note VARCHAR(255) NOT NULL DEFAULT '',
            CONSTRAINT uq_exclusive_email UNIQUE (email)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS ix_exclusive_user \
         ON exclusive_emails (user_id, status)",
    )
    .execute(pool)
    .await?;

    // 池子加一列标记：被独占的号不再参与临时邮箱的候选集。
    // 用 ALTER 加列（SQLite 支持），已存在时忽略错误。
    let _ = sqlx::query("ALTER TABLE email_accounts ADD COLUMN exclusive_owner INTEGER DEFAULT 0")
        .execute(pool)
        .await;

    Ok(())
}

/// 取一个独占邮箱（原子占位）。
///
/// 候选条件：
/// - `pool_status = 'available'`（没被别人临时占用）
/// - `health_status IN ('healthy','unknown')`
/// - **没被任何人独占**（`exclusive_owner = 0` 或 NULL）
/// - **没有临时占用记录**（`email_usages` 为空）——
///   已经被某项目用过的号不该再卖成「独占」，否则买家会收到别人的验证码
pub async fn claim_exclusive(
    pool: &SqlitePool,
    user_id: i64,
    provider: &str,
    price: i64,
) -> AppResult<Option<ExclusiveEmail>> {
    // 先清理超时占用（和临时邮箱共用一套回收）
    super::pool::recover_abandoned_claims(pool).await?;

    for _round in 0..5 {
        let candidate: Option<(i64, String)> = sqlx::query_as(
            "SELECT a.id, a.email FROM email_accounts a \
             WHERE a.provider = ? \
               AND a.pool_status = 'available' \
               AND a.health_status IN ('healthy','unknown') \
               AND COALESCE(a.exclusive_owner, 0) = 0 \
               AND NOT EXISTS (SELECT 1 FROM exclusive_emails e WHERE e.email = a.email AND e.status = 'active') \
               AND NOT EXISTS (SELECT 1 FROM email_usages u WHERE u.email = a.email) \
             ORDER BY a.last_check_at IS NULL, a.last_check_at DESC, a.id ASC \
             LIMIT 1",
        )
        .bind(provider)
        .fetch_optional(pool)
        .await?;

        let Some((acc_id, email)) = candidate else {
            return Ok(None);
        };

        // 原子占位：置为 exclusive + 记 owner
        let res = sqlx::query(
            "UPDATE email_accounts \
             SET pool_status = 'exclusive', status = 'exclusive', \
                 exclusive_owner = ?, claimed_at = ? \
             WHERE id = ? AND pool_status = 'available' \
               AND COALESCE(exclusive_owner, 0) = 0",
        )
        .bind(user_id)
        .bind(now_naive())
        .bind(acc_id)
        .execute(pool)
        .await?;

        if res.rows_affected() != 1 {
            continue; // 被抢走了，看下一个
        }

        // 写归属记录。
        //
        // ⚠ exclusive_emails.email 是 UNIQUE：释放过的邮箱会留一条
        // status='released' 的历史。再买同一个号时不能直接 INSERT
        // （会撞 UNIQUE），要先尝试「复活」旧记录。
        let now = now_naive();
        let revived = sqlx::query(
            "UPDATE exclusive_emails \
             SET user_id = ?, provider = ?, price = ?, status = 'active', \
                 created_at = ?, released_at = NULL, note = '' \
             WHERE email = ? AND status = 'released'",
        )
        .bind(user_id)
        .bind(provider)
        .bind(price)
        .bind(now)
        .bind(&email)
        .execute(pool)
        .await?;

        let r = if revived.rows_affected() == 1 {
            // 复活成功，取回 id
            let id: i64 = sqlx::query_scalar("SELECT id FROM exclusive_emails WHERE email = ?")
                .bind(&email)
                .fetch_one(pool)
                .await?;
            Ok::<i64, sqlx::Error>(id)
        } else {
            sqlx::query(
                "INSERT INTO exclusive_emails \
                 (user_id, email, provider, price, status, created_at, note) \
                 VALUES (?, ?, ?, ?, 'active', ?, '')",
            )
            .bind(user_id)
            .bind(&email)
            .bind(provider)
            .bind(price)
            .bind(now)
            .execute(pool)
            .await
            .map(|r| r.last_insert_rowid())
        };

        match r {
            Ok(id) => {
                tracing::info!(
                    "[exclusive] 用户 {} 买断 {} (id={})",
                    user_id, email, id
                );
                return Ok(Some(ExclusiveEmail {
                    id,
                    user_id,
                    email,
                    provider: provider.to_string(),
                    price,
                    status: "active".to_string(),
                    created_at: now,
                    released_at: None,
                    note: String::new(),
                }));
            }
            Err(e) => {
                // 归属记录写失败 → 回滚池状态，别把号卡死
                let _ = sqlx::query(
                    "UPDATE email_accounts SET pool_status = 'available', status = 'available', \
                     exclusive_owner = 0, claimed_at = NULL WHERE id = ?",
                )
                .bind(acc_id)
                .execute(pool)
                .await;
                return Err(AppError::internal(format!("写独占记录失败: {e}")));
            }
        }
    }

    Ok(None)
}

/// 释放独占邮箱（回池）。
pub async fn release_exclusive(pool: &SqlitePool, user_id: i64, email: &str) -> AppResult<bool> {
    let email = email.trim().to_ascii_lowercase();

    // 只能释放自己的
    let owned: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM exclusive_emails \
         WHERE lower(email) = ? AND user_id = ? AND status = 'active'",
    )
    .bind(&email)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;

    let Some(id) = owned else {
        return Ok(false);
    };

    // 1) 归属记录置为 released（保留历史，不删）
    sqlx::query(
        "UPDATE exclusive_emails SET status = 'released', released_at = ? WHERE id = ?",
    )
    .bind(now_naive())
    .bind(id)
    .execute(pool)
    .await?;

    // 2) 池子回 available，清 owner
    sqlx::query(
        "UPDATE email_accounts \
         SET pool_status = 'available', status = 'available', \
             exclusive_owner = 0, claimed_at = NULL \
         WHERE lower(email) = ?",
    )
    .bind(&email)
    .execute(pool)
    .await?;

    tracing::info!("[exclusive] 用户 {} 释放 {}", user_id, email);
    Ok(true)
}

/// 列出某个用户的独占邮箱。
pub async fn list_mine(
    pool: &SqlitePool,
    user_id: i64,
    include_released: bool,
) -> AppResult<Vec<ExclusiveEmail>> {
    let sql = if include_released {
        "SELECT * FROM exclusive_emails WHERE user_id = ? ORDER BY id DESC"
    } else {
        "SELECT * FROM exclusive_emails WHERE user_id = ? AND status = 'active' ORDER BY id DESC"
    };
    Ok(sqlx::query_as::<_, ExclusiveEmail>(sql)
        .bind(user_id)
        .fetch_all(pool)
        .await?)
}

/// 校验这个邮箱是否属于该用户且处于 active。
pub async fn owns(pool: &SqlitePool, user_id: i64, email: &str) -> AppResult<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM exclusive_emails \
         WHERE lower(email) = lower(?) AND user_id = ? AND status = 'active'",
    )
    .bind(email)
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

/// 统计：可售的独占邮箱数量（池子里没被任何方式占用过的）。
pub async fn count_available(pool: &SqlitePool, provider: &str) -> AppResult<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM email_accounts a \
         WHERE a.provider = ? \
           AND a.pool_status = 'available' \
           AND a.health_status IN ('healthy','unknown') \
           AND COALESCE(a.exclusive_owner, 0) = 0 \
           AND NOT EXISTS (SELECT 1 FROM exclusive_emails e WHERE e.email = a.email AND e.status = 'active') \
           AND NOT EXISTS (SELECT 1 FROM email_usages u WHERE u.email = a.email)",
    )
    .bind(provider)
    .fetch_one(pool)
    .await?)
}



/// 一次性买断扣费（不走「冻结→收码扣费」那套）。
///
/// 独占邮箱是**买断**语义：付钱就拿到，不依赖是否收到验证码。
/// 所以不能用 `billing::charge`（它是按订单的 hold_amount 扣，
/// 且要求 billing_status = 'pending'）。
///
/// 扣费 + 写流水在一个事务里，避免扣了钱没记录。
pub async fn charge_once(
    pool: &SqlitePool,
    user_id: i64,
    amount: i64,
    remark: &str,
) -> AppResult<()> {
    if amount <= 0 {
        return Ok(());
    }

    let mut tx = pool.begin().await?;

    let row: Option<(i64, i64, i64)> = sqlx::query_as(
        "SELECT balance, frozen_balance, total_charged FROM users WHERE id = ?",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some((before, frozen, charged)) = row else {
        return Err(AppError::not_found("用户不存在"));
    };

    let available = before - frozen;
    if available < amount {
        return Err(AppError::InsufficientBalance {
            need: crate::money::Money::from_milli(amount).to_yuan(),
            available: crate::money::Money::from_milli(available).to_yuan(),
        });
    }

    let now = now_naive();
    sqlx::query(
        "UPDATE users SET balance = balance - ?, total_charged = total_charged + ? WHERE id = ?",
    )
    .bind(amount)
    .bind(amount)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;

    // 余额流水（balance_logs）
    sqlx::query(
        "INSERT INTO balance_logs \
         (user_id, log_type, amount, balance_before, balance_after, \
          frozen_before, frozen_after, remark, created_at) \
         VALUES (?, 'charge', ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(-amount)
    .bind(before)
    .bind(before - amount)
    .bind(frozen)
    .bind(frozen)
    .bind(remark)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    let _ = charged;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    async fn setup() -> SqlitePool {
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
        crate::db::init_db(&pool).await.unwrap();
        ensure_schema(&pool).await.unwrap();
        pool
    }

    async fn add_account(pool: &SqlitePool, email: &str) {
        sqlx::query(
            "INSERT INTO email_accounts \
             (email, provider, account_json, status, pool_status, health_status, created_at) \
             VALUES (?, 'outlook', '{}', 'available', 'available', 'healthy', ?)",
        )
        .bind(email)
        .bind(now_naive())
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn claim_marks_account_exclusive() {
        let pool = setup().await;
        add_account(&pool, "a@x.test").await;

        let e = claim_exclusive(&pool, 1, "outlook", 500).await.unwrap().unwrap();
        assert_eq!(e.email, "a@x.test");
        assert!(e.is_active());

        // 池子状态应是 exclusive
        let st: String = sqlx::query_scalar("SELECT pool_status FROM email_accounts WHERE email='a@x.test'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(st, "exclusive");

        // 不能再被卖第二次
        let again = claim_exclusive(&pool, 2, "outlook", 500).await.unwrap();
        assert!(again.is_none(), "已独占的号不能再卖");
    }

    #[tokio::test]
    async fn release_returns_to_pool() {
        let pool = setup().await;
        add_account(&pool, "b@x.test").await;
        claim_exclusive(&pool, 1, "outlook", 500).await.unwrap().unwrap();

        assert!(release_exclusive(&pool, 1, "b@x.test").await.unwrap());

        let st: String = sqlx::query_scalar("SELECT pool_status FROM email_accounts WHERE email='b@x.test'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(st, "available", "释放后应回池");

        // 释放后别人可以买
        let e = claim_exclusive(&pool, 2, "outlook", 500).await.unwrap();
        assert!(e.is_some());
    }

    #[tokio::test]
    async fn cannot_release_others() {
        let pool = setup().await;
        add_account(&pool, "c@x.test").await;
        claim_exclusive(&pool, 1, "outlook", 500).await.unwrap().unwrap();

        // 用户 2 试图释放用户 1 的号
        assert!(!release_exclusive(&pool, 2, "c@x.test").await.unwrap());

        let st: String = sqlx::query_scalar("SELECT pool_status FROM email_accounts WHERE email='c@x.test'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(st, "exclusive", "不能释放别人的");
    }

    #[tokio::test]
    async fn skips_accounts_used_by_temp_projects() {
        let pool = setup().await;
        add_account(&pool, "used@x.test").await;
        add_account(&pool, "fresh@x.test").await;

        // used@x.test 被临时项目用过
        sqlx::query("INSERT INTO email_usages (email, project_code, first_used_at) VALUES ('used@x.test','p1',?)")
            .bind(now_naive()).execute(&pool).await.unwrap();

        let e = claim_exclusive(&pool, 1, "outlook", 500).await.unwrap().unwrap();
        assert_eq!(e.email, "fresh@x.test", "被临时项目用过的号不该卖成独占");
    }

    #[tokio::test]
    async fn owns_check() {
        let pool = setup().await;
        add_account(&pool, "d@x.test").await;
        claim_exclusive(&pool, 7, "outlook", 500).await.unwrap().unwrap();

        assert!(owns(&pool, 7, "d@x.test").await.unwrap());
        assert!(!owns(&pool, 8, "d@x.test").await.unwrap());
        assert!(!owns(&pool, 7, "nobody@x.test").await.unwrap());
    }

    #[tokio::test]
    async fn count_available_excludes_claimed() {
        let pool = setup().await;
        add_account(&pool, "e1@x.test").await;
        add_account(&pool, "e2@x.test").await;

        assert_eq!(count_available(&pool, "outlook").await.unwrap(), 2);
        claim_exclusive(&pool, 1, "outlook", 500).await.unwrap().unwrap();
        assert_eq!(count_available(&pool, "outlook").await.unwrap(), 1);
    }
}
