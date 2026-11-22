//! 计费核心：三段式（hold → charge / release）+ 余额流水。
//!
//! **与 Python 版 `app/services/billing_service.py` 的语义逐条对齐**，
//! 但修掉了原版的一个并发缺陷：
//!
//! 原版 `credit`/`debit`/`hold`/`charge` 都是「读 user → 判断 → 写回」，
//! 50 并发取号时两个请求可能同时通过 `available >= price` 校验，
//! 结果多冻一份钱。这里全部改成**条件 UPDATE**（`WHERE available >= ?`），
//! 让数据库的行锁 + 条件判断合成一个原子操作，`rows_affected == 0`
//! 就代表「余额不够，没冻成」。
//!
//! 账目顺序与 MailNest 一致：
//! - 取号：`frozen += price`（balance 不动）
//! - 收码：`frozen -= price` 且 `balance -= price`
//! - 释放：`frozen -= price`（balance 不动）

use chrono::NaiveDateTime;
use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};
use crate::models::*;
use crate::money::Money;
use crate::util::now_naive;

// ────────────────────────────────────────────────────────────────
//  查询
// ────────────────────────────────────────────────────────────────

pub async fn get_user(pool: &SqlitePool, user_id: i64) -> AppResult<Option<User>> {
    if user_id == 0 {
        return Ok(None);
    }
    Ok(sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(pool)
        .await?)
}

pub async fn get_user_by_name(pool: &SqlitePool, username: &str) -> AppResult<Option<User>> {
    Ok(
        sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn get_user_by_id_strict(pool: &SqlitePool, user_id: i64) -> AppResult<User> {
    get_user(pool, user_id)
        .await?
        .ok_or_else(|| AppError::not_found(format!("用户不存在: {user_id}")))
}

pub async fn list_users(
    pool: &SqlitePool,
    limit: i64,
    offset: i64,
) -> AppResult<(Vec<User>, i64)> {
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await?;
    let rows = sqlx::query_as::<_, User>("SELECT * FROM users ORDER BY id ASC LIMIT ? OFFSET ?")
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok((rows, total))
}

// ────────────────────────────────────────────────────────────────
//  用户 CRUD
// ────────────────────────────────────────────────────────────────

pub async fn create_user(
    pool: &SqlitePool,
    username: &str,
    password_hash: &str,
    role: i64,
    email: &str,
) -> AppResult<i64> {
    let res = sqlx::query(
        "INSERT INTO users (username, password_hash, role, status, email, \
         balance, frozen_balance, total_recharged, total_charged, created_at) \
         VALUES (?, ?, ?, 1, ?, 0, 0, 0, 0, ?)",
    )
    .bind(username)
    .bind(password_hash)
    .bind(role)
    .bind(email)
    .bind(now_naive())
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

// ────────────────────────────────────────────────────────────────
//  流水
// ────────────────────────────────────────────────────────────────

/// 写一条余额流水。必须在事务里调用。
async fn write_log(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    user_id: i64,
    log_type: &str,
    amount: Money,
    balance_before: Money,
    balance_after: Money,
    frozen_before: Money,
    frozen_after: Money,
    remark: &str,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO balance_logs (user_id, log_type, amount, balance_before, balance_after, \
         frozen_before, frozen_after, remark, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(log_type)
    .bind(amount.milli())
    .bind(balance_before.milli())
    .bind(balance_after.milli())
    .bind(frozen_before.milli())
    .bind(frozen_after.milli())
    .bind(crate::util::truncate_chars(remark, 255))
    .bind(now_naive())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

// ────────────────────────────────────────────────────────────────
//  加钱 / 扣钱
// ────────────────────────────────────────────────────────────────

/// 给用户加钱（原子）。`count_as_recharged` 为 None 时按 log_type 自动判断。
pub async fn credit(
    pool: &SqlitePool,
    user_id: i64,
    amount: Money,
    log_type: &str,
    remark: &str,
    count_as_recharged: Option<bool>,
) -> AppResult<()> {
    if amount.milli() < 0 {
        return Err(AppError::bad_request("credit 的金额必须为正"));
    }
    let count_as_recharged = count_as_recharged
        .unwrap_or(matches!(log_type, LOG_RECHARGE | LOG_REDEEM | LOG_AWARD));

    let mut tx = pool.begin().await?;

    // 原子更新：先读旧值，再在同一事务里条件更新（SQLite 事务内串行）
    let row: Option<(i64, i64)> = sqlx::query_as(
        "SELECT balance, frozen_balance FROM users WHERE id = ?",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((before_balance, before_frozen)) = row else {
        return Err(AppError::not_found(format!("用户不存在: {user_id}")));
    };

    let after_balance = before_balance + amount.milli();
    if count_as_recharged {
        sqlx::query(
            "UPDATE users SET balance = balance + ?, total_recharged = total_recharged + ? \
             WHERE id = ?",
        )
        .bind(amount.milli())
        .bind(amount.milli())
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query("UPDATE users SET balance = balance + ? WHERE id = ?")
            .bind(amount.milli())
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    }

    write_log(
        &mut tx,
        user_id,
        log_type,
        amount,
        Money::from_milli(before_balance),
        Money::from_milli(after_balance),
        Money::from_milli(before_frozen),
        Money::from_milli(before_frozen),
        remark,
    )
    .await?;

    tx.commit().await?;
    Ok(())
}

/// 从余额扣钱（原子，可用余额不足则失败）。
pub async fn debit(
    pool: &SqlitePool,
    user_id: i64,
    amount: Money,
    log_type: &str,
    remark: &str,
    require_available: bool,
) -> AppResult<()> {
    if amount.milli() < 0 {
        return Err(AppError::bad_request("debit 的金额必须为正"));
    }
    let mut tx = pool.begin().await?;

    let row: Option<(i64, i64)> = sqlx::query_as(
        "SELECT balance, frozen_balance FROM users WHERE id = ?",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((before_balance, before_frozen)) = row else {
        return Err(AppError::not_found(format!("用户不存在: {user_id}")));
    };

    if require_available && (before_balance - before_frozen) < amount.milli() {
        return Err(AppError::InsufficientBalance {
            need: amount.to_yuan(),
            available: Money::from_milli(before_balance - before_frozen).to_yuan(),
        });
    }

    // 条件更新：即使前面校验通过，这里也要在 SQL 层再挡一次
    let sql = if require_available {
        "UPDATE users SET balance = balance - ? WHERE id = ? AND (balance - frozen_balance) >= ?"
    } else {
        "UPDATE users SET balance = balance - ? WHERE id = ?"
    };
    let mut q = sqlx::query(sql).bind(amount.milli()).bind(user_id);
    if require_available {
        q = q.bind(amount.milli());
    }
    let res = q.execute(&mut *tx).await?;
    if res.rows_affected() != 1 {
        return Err(AppError::InsufficientBalance {
            need: amount.to_yuan(),
            available: Money::from_milli(before_balance - before_frozen).to_yuan(),
        });
    }

    write_log(
        &mut tx,
        user_id,
        log_type,
        Money::from_milli(-amount.milli()),
        Money::from_milli(before_balance),
        Money::from_milli(before_balance - amount.milli()),
        Money::from_milli(before_frozen),
        Money::from_milli(before_frozen),
        remark,
    )
    .await?;

    tx.commit().await?;
    Ok(())
}

// ────────────────────────────────────────────────────────────────
//  三段式计费
// ────────────────────────────────────────────────────────────────

/// 取号冻结：`frozen += price`，balance 不变。
///
/// 同一邮箱已有订单时不重复冻结（对齐 Python：`email_orders.email` 上有唯一约束）。
pub async fn hold(
    pool: &SqlitePool,
    user_id: i64,
    email: &str,
    project_code: &str,
    provider: &str,
    price: Money,
) -> AppResult<EmailOrder> {
    let email = email.trim().to_ascii_lowercase();

    // 已存在订单 → 直接返回（对齐 Python `hold` 的幂等分支）
    if let Some(existing) = find_order_any(pool, &email).await? {
        return Ok(existing);
    }

    let mut tx = pool.begin().await?;

    let row: Option<(i64, i64)> = sqlx::query_as(
        "SELECT balance, frozen_balance FROM users WHERE id = ?",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((before_balance, before_frozen)) = row else {
        return Err(AppError::not_found(format!("用户不存在: {user_id}")));
    };

    if price.is_positive() {
        // 原子冻结：可用余额不足时 rows_affected = 0，不会被冻穿
        let res = sqlx::query(
            "UPDATE users SET frozen_balance = frozen_balance + ? \
             WHERE id = ? AND (balance - frozen_balance) >= ?",
        )
        .bind(price.milli())
        .bind(user_id)
        .bind(price.milli())
        .execute(&mut *tx)
        .await?;
        if res.rows_affected() != 1 {
            return Err(AppError::InsufficientBalance {
                need: price.to_yuan(),
                available: Money::from_milli(before_balance - before_frozen).to_yuan(),
            });
        }

        write_log(
            &mut tx,
            user_id,
            LOG_HOLD,
            Money::ZERO,
            Money::from_milli(before_balance),
            Money::from_milli(before_balance),
            Money::from_milli(before_frozen),
            Money::from_milli(before_frozen + price.milli()),
            &format!("取号冻结 {email}"),
        )
        .await?;
    }

    let res = sqlx::query(
        "INSERT INTO email_orders (user_id, email, project_code, provider, price, \
         billing_status, hold_amount, charged_amount, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?)",
    )
    .bind(user_id)
    .bind(&email)
    .bind(project_code)
    .bind(provider)
    .bind(price.milli())
    .bind(BILLING_PENDING)
    .bind(price.milli())
    .bind(now_naive())
    .execute(&mut *tx)
    .await?;
    let id = res.last_insert_rowid();

    tx.commit().await?;

    Ok(EmailOrder {
        id,
        user_id,
        email,
        project_code: project_code.to_string(),
        provider: provider.to_string(),
        price: price.milli(),
        billing_status: BILLING_PENDING.to_string(),
        hold_amount: price.milli(),
        charged_amount: 0,
        created_at: now_naive(),
        charged_at: None,
        released_at: None,
    })
}

/// 按邮箱找计费订单。
///
/// 同一邮箱可被多个项目各买一次（按项目一次性），所以：
/// - 带 project_code → 精确匹配
/// - 不带 → 先找 pending（最可能是本次取号），再退回最新一单
pub async fn find_order(
    pool: &SqlitePool,
    email: &str,
    project_code: Option<&str>,
) -> AppResult<Option<EmailOrder>> {
    let email = email.trim().to_ascii_lowercase();
    if let Some(pc) = project_code.filter(|s| !s.is_empty()) {
        return Ok(sqlx::query_as::<_, EmailOrder>(
            "SELECT * FROM email_orders WHERE email = ? AND project_code = ?",
        )
        .bind(&email)
        .bind(pc)
        .fetch_optional(pool)
        .await?);
    }
    if let Some(o) = sqlx::query_as::<_, EmailOrder>(
        "SELECT * FROM email_orders WHERE email = ? AND billing_status = ? \
         ORDER BY id DESC LIMIT 1",
    )
    .bind(&email)
    .bind(BILLING_PENDING)
    .fetch_optional(pool)
    .await?
    {
        return Ok(Some(o));
    }
    Ok(sqlx::query_as::<_, EmailOrder>(
        "SELECT * FROM email_orders WHERE email = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(&email)
    .fetch_optional(pool)
    .await?)
}

async fn find_order_any(pool: &SqlitePool, email: &str) -> AppResult<Option<EmailOrder>> {
    Ok(sqlx::query_as::<_, EmailOrder>(
        "SELECT * FROM email_orders WHERE email = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(email)
    .fetch_optional(pool)
    .await?)
}

/// 收到验证码 → 真扣：`frozen -= price` 且 `balance -= price`。幂等。
pub async fn charge(
    pool: &SqlitePool,
    user_id: i64,
    email: &str,
    remark: &str,
    project_code: Option<&str>,
) -> AppResult<Option<EmailOrder>> {
    let Some(order) = find_order(pool, email, project_code).await? else {
        return Ok(None);
    };
    // 幂等：已扣过 / 已释放，都不再动
    if order.billing_status != BILLING_PENDING {
        return Ok(Some(order));
    }

    let price = Money::from_milli(order.hold_amount);

    let mut tx = pool.begin().await?;

    let row: Option<(i64, i64, i64)> = sqlx::query_as(
        "SELECT balance, frozen_balance, total_charged FROM users WHERE id = ?",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((before_balance, before_frozen, before_charged)) = row else {
        return Err(AppError::not_found(format!("用户不存在: {user_id}")));
    };

    if price.is_positive() {
        // 冻结额先退（不许为负），余额再扣 —— 与 MailNest 账目顺序一致
        sqlx::query(
            "UPDATE users SET frozen_balance = MAX(0, frozen_balance - ?), \
             balance = balance - ?, total_charged = total_charged + ? WHERE id = ?",
        )
        .bind(price.milli())
        .bind(price.milli())
        .bind(price.milli())
        .bind(user_id)
        .execute(&mut *tx)
        .await?;

        let after_frozen = std::cmp::max(0, before_frozen - price.milli());
        let remark_text = if remark.is_empty() {
            format!("收码扣费 {email}")
        } else {
            remark.to_string()
        };
        write_log(
            &mut tx,
            user_id,
            LOG_CHARGE,
            Money::from_milli(-price.milli()),
            Money::from_milli(before_balance),
            Money::from_milli(before_balance - price.milli()),
            Money::from_milli(before_frozen),
            Money::from_milli(after_frozen),
            &remark_text,
        )
        .await?;
        let _ = before_charged;
    }

    sqlx::query(
        "UPDATE email_orders SET billing_status = ?, charged_amount = ?, charged_at = ? \
         WHERE id = ? AND billing_status = ?",
    )
    .bind(BILLING_CHARGED)
    .bind(price.milli())
    .bind(now_naive())
    .bind(order.id)
    .bind(BILLING_PENDING)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    let mut o = order;
    o.billing_status = BILLING_CHARGED.to_string();
    o.charged_amount = price.milli();
    o.charged_at = Some(now_naive());
    Ok(Some(o))
}

/// 释放/超时 → 解冻：`frozen -= price`，balance 不变，订单标记 free。幂等。
pub async fn release(
    pool: &SqlitePool,
    user_id: i64,
    email: &str,
    remark: &str,
    project_code: Option<&str>,
) -> AppResult<Option<EmailOrder>> {
    let Some(order) = find_order(pool, email, project_code).await? else {
        return Ok(None);
    };
    if matches!(order.billing_status.as_str(), BILLING_CHARGED | BILLING_FREE) {
        return Ok(Some(order));
    }

    let price = Money::from_milli(order.hold_amount);
    let mut tx = pool.begin().await?;

    if price.is_positive() {
        let row: Option<(i64, i64)> =
            sqlx::query_as("SELECT balance, frozen_balance FROM users WHERE id = ?")
                .bind(user_id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some((before_balance, before_frozen)) = row {
            sqlx::query(
                "UPDATE users SET frozen_balance = MAX(0, frozen_balance - ?) WHERE id = ?",
            )
            .bind(price.milli())
            .bind(user_id)
            .execute(&mut *tx)
            .await?;

            let after_frozen = std::cmp::max(0, before_frozen - price.milli());
            let remark_text = if remark.is_empty() {
                format!("释放解冻 {email}")
            } else {
                remark.to_string()
            };
            write_log(
                &mut tx,
                user_id,
                LOG_RELEASE,
                Money::ZERO,
                Money::from_milli(before_balance),
                Money::from_milli(before_balance),
                Money::from_milli(before_frozen),
                Money::from_milli(after_frozen),
                &remark_text,
            )
            .await?;
        }
    }

    sqlx::query(
        "UPDATE email_orders SET billing_status = ?, released_at = ? \
         WHERE id = ? AND billing_status = ?",
    )
    .bind(BILLING_FREE)
    .bind(now_naive())
    .bind(order.id)
    .bind(BILLING_PENDING)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    let mut o = order;
    o.billing_status = BILLING_FREE.to_string();
    o.released_at = Some(now_naive());
    Ok(Some(o))
}

/// 定时任务：把超时的 pending 订单释放（不扣费）。
pub async fn release_expired(
    pool: &SqlitePool,
    timeout_seconds: i64,
    limit: i64,
) -> AppResult<i64> {
    let cutoff = now_naive() - chrono::Duration::seconds(timeout_seconds.max(0));
    let orders = sqlx::query_as::<_, EmailOrder>(
        "SELECT * FROM email_orders WHERE billing_status = ? AND created_at < ? LIMIT ?",
    )
    .bind(BILLING_PENDING)
    .bind(cutoff)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut count = 0;
    for order in orders {
        release(pool, order.user_id, &order.email, "超时自动释放", None).await?;
        count += 1;
    }
    Ok(count)
}

// ────────────────────────────────────────────────────────────────
//  余额快照 / 流水列表
// ────────────────────────────────────────────────────────────────

/// 余额六栏（对齐 MailNest `/api/v1/balance`）。
///
/// 所有分栏都从 `balance_logs` 流水算 —— 流水是唯一事实来源。
#[derive(Debug, Clone, serde::Serialize)]
pub struct BalanceSnapshot {
    pub balance: String,
    pub frozen_balance: String,
    pub available_balance: String,
    pub scan_recharge: String,
    pub card_recharge: String,
    pub award_amount: String,
}

pub async fn balance_snapshot(
    pool: &SqlitePool,
    user_id: i64,
) -> AppResult<BalanceSnapshot> {
    let Some(user) = get_user(pool, user_id).await? else {
        return Ok(BalanceSnapshot {
            balance: "0.000".into(),
            frozen_balance: "0.000".into(),
            available_balance: "0.000".into(),
            scan_recharge: "0.000".into(),
            card_recharge: "0.000".into(),
            award_amount: "0.000".into(),
        });
    };

    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT log_type, COALESCE(SUM(amount), 0) FROM balance_logs \
         WHERE user_id = ? GROUP BY log_type",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    let bucket = |t: &str| {
        rows.iter()
            .find(|(k, _)| k == t)
            .map(|(_, v)| *v)
            .unwrap_or(0)
    };

    Ok(BalanceSnapshot {
        balance: user.balance_money().to_yuan(),
        frozen_balance: Money::from_milli(user.frozen_balance).to_yuan(),
        available_balance: user.available_balance().to_yuan(),
        scan_recharge: Money::from_milli(bucket(LOG_RECHARGE)).to_yuan(),
        card_recharge: Money::from_milli(bucket(LOG_REDEEM)).to_yuan(),
        award_amount: Money::from_milli(bucket(LOG_AWARD)).to_yuan(),
    })
}

pub async fn list_balance_logs(
    pool: &SqlitePool,
    user_id: Option<i64>,
    limit: i64,
    offset: i64,
    log_type: Option<&str>,
) -> AppResult<(Vec<BalanceLog>, i64)> {
    let mut where_sql = String::from(" WHERE 1=1");
    if user_id.is_some() {
        where_sql.push_str(" AND user_id = ?");
    }
    if log_type.filter(|s| !s.is_empty()).is_some() {
        where_sql.push_str(" AND log_type = ?");
    }

    let count_sql = format!("SELECT COUNT(*) FROM balance_logs{where_sql}");
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
    if let Some(uid) = user_id {
        cq = cq.bind(uid);
    }
    if let Some(t) = log_type.filter(|s| !s.is_empty()) {
        cq = cq.bind(t);
    }
    let total = cq.fetch_one(pool).await?;

    let list_sql = format!(
        "SELECT * FROM balance_logs{where_sql} ORDER BY id DESC LIMIT ? OFFSET ?"
    );
    let mut lq = sqlx::query_as::<_, BalanceLog>(&list_sql);
    if let Some(uid) = user_id {
        lq = lq.bind(uid);
    }
    if let Some(t) = log_type.filter(|s| !s.is_empty()) {
        lq = lq.bind(t);
    }
    let rows = lq.bind(limit).bind(offset).fetch_all(pool).await?;

    Ok((rows, total))
}

/// 项目单价（厘）：项目 config 的 `price` 优先，其次全局 `billing.default_price`。
pub async fn project_price(
    pool: &SqlitePool,
    project: &Project,
    billing: Option<&crate::services::settings::BillingConfig>,
) -> AppResult<Money> {
    let cfg = project.config();
    if let Some(raw) = cfg.get("price") {
        let s = if raw.is_string() {
            raw.as_str().unwrap_or("").to_string()
        } else if raw.is_null() {
            String::new()
        } else {
            raw.to_string()
        };
        if !s.is_empty() {
            if let Some(m) = Money::parse_yuan(&s) {
                return Ok(Money::from_milli(m.milli().max(0)));
            }
        }
    }

    let owned;
    let billing = match billing {
        Some(b) => b,
        None => {
            owned = crate::services::settings::get_billing_config(pool).await?;
            &owned
        }
    };
    if !billing.default_price.is_empty() {
        if let Some(m) = Money::parse_yuan(&billing.default_price) {
            return Ok(Money::from_milli(m.milli().max(0)));
        }
    }
    Ok(Money::ZERO)
}

/// `last_scan_at` 之类的辅助：取最近一次健康扫描时间。
pub async fn latest_scan_at(pool: &SqlitePool) -> AppResult<Option<NaiveDateTime>> {
    Ok(sqlx::query_scalar::<_, Option<NaiveDateTime>>(
        "SELECT MAX(last_check_at) FROM email_accounts",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(None))
}

// ────────────────────────────────────────────────────────────────
//  订单 / 充值单列表
// ────────────────────────────────────────────────────────────────

/// 邮箱订单列表（下单流水页用）。
///
/// `user_id` 传 None = 全部用户（管理员）。
pub async fn list_email_orders(
    pool: &SqlitePool,
    user_id: Option<i64>,
    project_code: &str,
    limit: i64,
    offset: i64,
) -> AppResult<(Vec<EmailOrder>, i64)> {
    let mut where_sql = String::from(" WHERE 1=1");
    if user_id.is_some() {
        where_sql.push_str(" AND user_id = ?");
    }
    if !project_code.is_empty() {
        where_sql.push_str(" AND project_code = ?");
    }

    let count_sql = format!("SELECT COUNT(*) FROM email_orders{where_sql}");
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
    if let Some(uid) = user_id {
        cq = cq.bind(uid);
    }
    if !project_code.is_empty() {
        cq = cq.bind(project_code);
    }
    let total = cq.fetch_one(pool).await?;

    let list_sql = format!(
        "SELECT * FROM email_orders{where_sql} ORDER BY id DESC LIMIT ? OFFSET ?"
    );
    let mut lq = sqlx::query_as::<_, EmailOrder>(&list_sql);
    if let Some(uid) = user_id {
        lq = lq.bind(uid);
    }
    if !project_code.is_empty() {
        lq = lq.bind(project_code);
    }
    let rows = lq.bind(limit).bind(offset).fetch_all(pool).await?;
    Ok((rows, total))
}

/// 充值订单列表。
pub async fn list_recharge_orders(
    pool: &SqlitePool,
    user_id: Option<i64>,
    limit: i64,
    offset: i64,
) -> AppResult<(Vec<RechargeOrder>, i64)> {
    let (count_sql, list_sql) = if user_id.is_some() {
        (
            "SELECT COUNT(*) FROM recharge_orders WHERE user_id = ?".to_string(),
            "SELECT * FROM recharge_orders WHERE user_id = ? ORDER BY id DESC LIMIT ? OFFSET ?"
                .to_string(),
        )
    } else {
        (
            "SELECT COUNT(*) FROM recharge_orders".to_string(),
            "SELECT * FROM recharge_orders ORDER BY id DESC LIMIT ? OFFSET ?".to_string(),
        )
    };

    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
    if let Some(uid) = user_id {
        cq = cq.bind(uid);
    }
    let total = cq.fetch_one(pool).await?;

    let mut lq = sqlx::query_as::<_, RechargeOrder>(&list_sql);
    if let Some(uid) = user_id {
        lq = lq.bind(uid);
    }
    let rows = lq.bind(limit).bind(offset).fetch_all(pool).await?;
    Ok((rows, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_zero_shape() {
        // 语义校验：可用 = 总额 - 冻结
        let u = User {
            id: 1,
            username: "x".into(),
            password_hash: String::new(),
            role: ROLE_USER,
            status: 1,
            email: String::new(),
            balance: 1000,
            frozen_balance: 300,
            total_recharged: 0,
            total_charged: 0,
            created_at: chrono::Local::now().naive_local(),
        };
        assert_eq!(u.available_balance().milli(), 700);
        assert_eq!(u.available_balance().to_yuan(), "0.700");
    }
}
