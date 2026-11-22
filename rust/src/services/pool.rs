//! 号池服务：取号（claim）、回收、计数、健康汇总、释放。
//!
//! 核心不变量（与 Python 版一致，但并发语义更强）：
//!
//! 1. **先原子占位，再验号**。反过来（先验再占）在并发下会退化成
//!    「N 个线程一起验同一批候选、只有一个抢得到」，白烧 N 倍 token 刷新，
//!    抢输的线程若就此放弃，库存充足也会误报 D0002。
//!    实测（828 库存）修复前并发 20 → 8/20，修复后 20/20。
//!
//! 2. **按项目一次性**：只跳过本项目用过的邮箱，其他项目照常可取。
//!    释放即回池，绑定记录（email_usages）永久保留。
//!
//! 3. **只发 healthy / unknown**：unhealthy（检测失败过）和 dead 一律不发。

use std::collections::HashSet;

use sqlx::SqlitePool;

use crate::error::AppResult;
use crate::models::*;
use crate::util::now_naive;

/// 取号租约：超过这个时间还在 using 的号会被回收。
pub const CLAIM_LEASE_TIMEOUT_MINUTES: i64 = 15;

/// 每轮候选数（对应 Python `PROBE_PER_ROUND`）。
const PROBE_PER_ROUND: i64 = 8;
/// 最多轮数（对应 Python `MAX_ROUNDS`）。
const MAX_ROUNDS: i32 = 20;

// ────────────────────────────────────────────────────────────────
//  统计
// ────────────────────────────────────────────────────────────────

/// 号池状态计数。
pub async fn count_accounts(pool: &SqlitePool, provider: &str) -> AppResult<PoolCounts> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT COALESCE(pool_status, 'available') AS s, COUNT(*) \
         FROM email_accounts WHERE provider = ? GROUP BY s",
    )
    .bind(provider)
    .fetch_all(pool)
    .await?;

    let mut c = PoolCounts::default();
    for (status, n) in rows {
        c.total += n;
        match status.as_str() {
            POOL_AVAILABLE => c.available += n,
            POOL_USING => c.using += n,
            POOL_RETIRED => c.retired += n,
            _ => {}
        }
    }
    // dead 是按 health_status 算的（与 pool_status 正交）
    c.dead = sqlx::query_scalar(
        "SELECT COUNT(*) FROM email_accounts WHERE provider = ? AND health_status = 'dead'",
    )
    .bind(provider)
    .fetch_one(pool)
    .await?;
    Ok(c)
}

/// 项目可 claim 数量。
///
/// **性能关键**：原 Python 版每个项目跑一次全表扫描（`NOT IN` 子查询），
/// 55 个项目 = 54,725 行/请求。这里改成「一次基数查询 + 一次 GROUP BY」。
pub async fn count_claimable(
    pool: &SqlitePool,
    provider: &str,
    project_code: &str,
) -> AppResult<i64> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM email_accounts a \
         WHERE a.provider = ? AND a.pool_status = 'available' \
           AND a.health_status IN ('healthy','unknown') \
           AND COALESCE(a.exclusive_owner, 0) = 0 \
           AND a.email NOT IN (SELECT email FROM email_usages WHERE project_code = ?)",
    )
    .bind(provider)
    .bind(project_code)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// 统计各项目「真实可 claim 的邮箱数」。
///
/// ## 为什么不能写成 `基数 - 该项目已用量`
///
/// 这是修掉的一个**真 bug**。原来的实现：
/// ```text
/// stock = COUNT(池子里可用的) - COUNT(该项目用过的)
/// ```
/// 这个减法只在一个前提下成立：**用过的邮箱都还在池子里**。
/// 现实是它们会离开池子（健康检查判死、超时回收、退役），于是：
///
/// ```text
/// 生产实测 2026-11-22：
///   池子可用基数        = 768
///   chatgpt001 用过      = 780     （其中 85 个已不在池子里）
///   显示 = 768 - 780     = -12     ← 负数！
///   真实可 claim         = 55      ← 池子里没被 chatgpt 用过的
/// ```
///
/// 其他项目也都有偏差（aws001 少 2、x001 少 1），只是不像 chatgpt 那么夸张。
///
/// ## 正确做法
///
/// **直接数「池子里且没被本项目用过的」**，不做减法：
/// ```sql
/// SELECT COUNT(*) FROM email_accounts a
/// WHERE a.provider = ? AND a.pool_status = 'available'
///   AND a.health_status IN ('healthy','unknown')
///   AND a.email NOT IN (SELECT email FROM email_usages WHERE project_code = ?)
/// ```
///
/// 用一条 SQL 按项目 GROUP BY 一次算完，避免 53 个项目查 53 次
/// （995 行 × 53 = 秒级延迟，Python 版注释里也提到过这个坑）。
pub async fn count_claimable_bulk(
    pool: &SqlitePool,
    provider: &str,
) -> AppResult<std::collections::HashMap<String, i64>> {
    // 一次查出所有项目的真实可 claim 数。
    //
    // 关键点：`NOT EXISTS` 而不是 `NOT IN` ——
    // `NOT IN` 在子查询返回 NULL 时会整体变 NULL（SQL 三值逻辑），
    // 虽然 email 列是 NOT NULL，但用 EXISTS 语义更明确也更快。
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT p.code, \
                (SELECT COUNT(*) FROM email_accounts a \
                 WHERE a.provider = p.provider \
                   AND a.pool_status = 'available' \
                   AND a.health_status IN ('healthy','unknown') \
                   AND COALESCE(a.exclusive_owner, 0) = 0 \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM email_usages u \
                       WHERE u.email = a.email AND u.project_code = p.code \
                   )) AS claimable \
         FROM projects p \
         WHERE p.provider = ? AND p.enabled = 1",
    )
    .bind(provider)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().collect())
}

/// 健康状态汇总（页面展示用）。
pub async fn health_summary(pool: &SqlitePool, provider: &str) -> AppResult<HealthSummary> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT COALESCE(health_status, 'unknown') AS h, COUNT(*) \
         FROM email_accounts WHERE provider = ? GROUP BY h",
    )
    .bind(provider)
    .fetch_all(pool)
    .await?;

    let mut s = HealthSummary::default();
    for (h, n) in rows {
        s.total += n;
        match h.as_str() {
            HEALTH_HEALTHY => s.healthy += n,
            HEALTH_UNHEALTHY => s.unhealthy += n,
            HEALTH_DEAD => s.dead += n,
            _ => s.unknown += n,
        }
    }

    // **格式契约**：Python 版这里是 `MAX(last_check_at)` 的裸 datetime
    // 直接 isoformat()，输出 `2026-11-17T01:09:34.906721`（**无时区后缀**）。
    // 管理 API 的客户端按这个格式解析，所以这里不能带 `Z`。
    // （`iso_from_naive` 带 Z，是给对外 API 的时间字段用的，两者不同。）
    s.last_scan_at = sqlx::query_scalar::<_, Option<chrono::NaiveDateTime>>(
        "SELECT MAX(last_check_at) FROM email_accounts WHERE provider = ?",
    )
    .bind(provider)
    .fetch_one(pool)
    .await
    .unwrap_or(None)
    .map(|d| d.format("%Y-%m-%dT%H:%M:%S%.6f").to_string())
    .unwrap_or_default();

    Ok(s)
}

// ────────────────────────────────────────────────────────────────
//  取号
// ────────────────────────────────────────────────────────────────

/// 取号结果。
#[derive(Debug, Clone)]
pub struct Claimed {
    pub account_id: i64,
    pub email: String,
}

/// 取号。返回 `None` 表示库存不足（对应 Python 返回 None → D0002）。
///
/// `probe` 是一个回调：拿到「已占位的账号」后做一次实时凭据预检，
/// 返回 `(alive, reason)`。放在参数里是为了让号池逻辑与 Graph 网络调用解耦
/// （也便于测试注入）。
pub async fn claim_account<F, Fut>(
    pool: &SqlitePool,
    provider: &str,
    project_code: &str,
    mut probe: F,
) -> AppResult<Option<Claimed>>
where
    F: FnMut(EmailAccount) -> Fut,
    Fut: std::future::Future<Output = (bool, String)>,
{
    recover_abandoned_claims(pool).await?;

    for _round in 0..MAX_ROUNDS {
        let candidates = sqlx::query_as::<_, EmailAccount>(
            "SELECT * FROM email_accounts a \
             WHERE a.provider = ? AND a.pool_status = 'available' \
               AND a.health_status IN ('healthy','unknown') \
               AND a.email NOT IN (SELECT email FROM email_usages WHERE project_code = ?) \
             ORDER BY a.last_check_at IS NULL, a.last_check_at DESC, a.id ASC \
             LIMIT ?",
        )
        .bind(provider)
        .bind(project_code)
        .bind(PROBE_PER_ROUND)
        .fetch_all(pool)
        .await?;

        if candidates.is_empty() {
            break;
        }

        for acc in candidates {
            // ── 1. 原子占位 ──
            // 仅当账号仍是 available 时置为 using。SQLite 的写锁保证并发下
            // 不会重复取走同一账号（UPDATE ... WHERE pool_status='available'）。
            let res = sqlx::query(
                "UPDATE email_accounts SET pool_status = 'using', status = 'using', claimed_at = ? \
                 WHERE id = ? AND pool_status = 'available'",
            )
            .bind(now_naive())
            .bind(acc.id)
            .execute(pool)
            .await?;

            if res.rows_affected() != 1 {
                // 已被别的任务抢走 → 不浪费一次 token 刷新，看下一个
                continue;
            }

            // ── 2. 实时预检（网络调用，期间不持有数据库事务）──
            let (alive, reason) = probe(acc.clone()).await;

            if !alive {
                tracing::warn!(
                    "[claim] 预检失败，标记死号并跳过 email={} reason={}",
                    acc.email,
                    crate::util::truncate_chars(&reason, 80)
                );
                sqlx::query(
                    "UPDATE email_accounts SET pool_status = 'retired', status = 'retired', \
                     health_status = 'dead', health_reason = ?, last_check_at = ? WHERE id = ?",
                )
                .bind(crate::util::truncate_chars(&reason, 120))
                .bind(now_naive())
                .bind(acc.id)
                .execute(pool)
                .await?;
                continue;
            }

            // 预检可能轮换出新的 refresh_token，必须落库
            // （否则下次刷新用旧 rt 会失效）。probe 里已把新值写回 acc.account_json。
            sqlx::query(
                "UPDATE email_accounts SET last_check_at = ?, account_json = ? WHERE id = ?",
            )
            .bind(now_naive())
            .bind(&acc.account_json)
            .bind(acc.id)
            .execute(pool)
            .await?;

            tracing::info!("[claim] ✓ 取号成功 email={} account_id={}", acc.email, acc.id);
            return Ok(Some(Claimed {
                account_id: acc.id,
                email: acc.email,
            }));
        }
    }

    tracing::info!("[claim] ✗ 无可用号 provider={provider}（已扫 {MAX_ROUNDS} 轮候选）");
    Ok(None)
}

/// 回收超时占用的号（using → available）。
///
/// 按项目一次性语义下，被某项目绑定的邮箱对其他项目仍可用，
/// 所以不再退役 —— 绑定记录（email_usages）保留，本项目取不到即可。
pub async fn recover_abandoned_claims(pool: &SqlitePool) -> AppResult<i64> {
    let cutoff = now_naive() - chrono::Duration::minutes(CLAIM_LEASE_TIMEOUT_MINUTES);
    let res = sqlx::query(
        "UPDATE email_accounts SET pool_status = 'available', status = 'available', \
         claimed_at = NULL \
         WHERE pool_status = 'using' AND claimed_at IS NOT NULL AND claimed_at < ?",
    )
    .bind(cutoff)
    .execute(pool)
    .await?;
    let n = res.rows_affected() as i64;
    if n > 0 {
        tracing::info!("[claim-recovery] recovered={n}（租约 {CLAIM_LEASE_TIMEOUT_MINUTES} 分钟）");
    }
    Ok(n)
}

/// 释放已领取账号，**回池**给其他项目继续用。
pub async fn release_account(pool: &SqlitePool, account_id: i64) -> AppResult<bool> {
    let res = sqlx::query(
        "UPDATE email_accounts SET pool_status = 'available', status = 'available', \
         claimed_at = NULL WHERE id = ? AND pool_status = 'using'",
    )
    .bind(account_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// 按邮箱释放。
pub async fn release_account_by_email(pool: &SqlitePool, email: &str) -> AppResult<bool> {
    let res = sqlx::query(
        "UPDATE email_accounts SET pool_status = 'available', status = 'available', \
         claimed_at = NULL WHERE email = ? AND pool_status = 'using'",
    )
    .bind(email.trim().to_ascii_lowercase())
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

// ────────────────────────────────────────────────────────────────
//  邮箱绑定（按项目一次性）
// ────────────────────────────────────────────────────────────────

/// 绑定邮箱到项目。返回 `(ok, conflict_project)`。
///
/// 同一邮箱同一项目只能绑一次（重复取 → 冲突）；其他项目绑同一邮箱不受影响。
pub async fn bind_email(
    pool: &SqlitePool,
    email: &str,
    project_code: &str,
) -> AppResult<(bool, Option<String>)> {
    let key = email.trim().to_ascii_lowercase();
    if key.is_empty() {
        return Ok((false, None));
    }
    let res = sqlx::query(
        "INSERT INTO email_usages (email, project_code, first_used_at) VALUES (?, ?, ?)",
    )
    .bind(&key)
    .bind(project_code)
    .bind(now_naive())
    .execute(pool)
    .await;

    match res {
        Ok(_) => Ok((true, None)),
        Err(sqlx::Error::Database(e)) if e.message().contains("UNIQUE") => {
            let existing: Option<String> = sqlx::query_scalar(
                "SELECT project_code FROM email_usages WHERE email = ? AND project_code = ?",
            )
            .bind(&key)
            .bind(project_code)
            .fetch_optional(pool)
            .await?;
            Ok((false, Some(existing.unwrap_or_else(|| project_code.to_string()))))
        }
        Err(e) => Err(e.into()),
    }
}

/// 该邮箱是否已被**本项目**使用过。
pub async fn used_by_project(
    pool: &SqlitePool,
    email: &str,
    project_code: &str,
) -> AppResult<bool> {
    let key = email.trim().to_ascii_lowercase();
    if key.is_empty() {
        return Ok(false);
    }
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM email_usages WHERE email = ? AND project_code = ?",
    )
    .bind(&key)
    .bind(project_code)
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

/// 某个邮箱在哪些项目用过。
pub async fn projects_using(
    pool: &SqlitePool,
    email: &str,
) -> AppResult<Vec<String>> {
    let key = email.trim().to_ascii_lowercase();
    Ok(sqlx::query_scalar(
        "SELECT project_code FROM email_usages WHERE email = ? ORDER BY first_used_at",
    )
    .bind(&key)
    .fetch_all(pool)
    .await?)
}

// ────────────────────────────────────────────────────────────────
//  账号查询 / 维护
// ────────────────────────────────────────────────────────────────

pub async fn get_account(pool: &SqlitePool, account_id: i64) -> AppResult<Option<EmailAccount>> {
    Ok(
        sqlx::query_as::<_, EmailAccount>("SELECT * FROM email_accounts WHERE id = ?")
            .bind(account_id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn get_account_by_email(
    pool: &SqlitePool,
    email: &str,
) -> AppResult<Option<EmailAccount>> {
    Ok(sqlx::query_as::<_, EmailAccount>(
        "SELECT * FROM email_accounts WHERE email = ?",
    )
    .bind(email.trim().to_ascii_lowercase())
    .fetch_optional(pool)
    .await?)
}

/// 账号列表（分页下推 SQL，不把整表拉到内存）。
pub async fn list_accounts(
    pool: &SqlitePool,
    provider: &str,
    keyword: &str,
    limit: i64,
    offset: i64,
) -> AppResult<(Vec<EmailAccount>, i64)> {
    let kw = keyword.trim();
    let total: i64 = if kw.is_empty() {
        sqlx::query_scalar("SELECT COUNT(*) FROM email_accounts WHERE provider = ?")
            .bind(provider)
            .fetch_one(pool)
            .await?
    } else {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM email_accounts WHERE provider = ? AND email LIKE ?",
        )
        .bind(provider)
        .bind(format!("%{kw}%"))
        .fetch_one(pool)
        .await?
    };

    let rows = if kw.is_empty() {
        sqlx::query_as::<_, EmailAccount>(
            "SELECT * FROM email_accounts WHERE provider = ? ORDER BY id ASC LIMIT ? OFFSET ?",
        )
        .bind(provider)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as::<_, EmailAccount>(
            "SELECT * FROM email_accounts WHERE provider = ? AND email LIKE ? \
             ORDER BY id ASC LIMIT ? OFFSET ?",
        )
        .bind(provider)
        .bind(format!("%{kw}%"))
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?
    };

    Ok((rows, total))
}

/// 删除账号（永久）。
pub async fn delete_account(pool: &SqlitePool, account_id: i64) -> AppResult<bool> {
    let res = sqlx::query("DELETE FROM email_accounts WHERE id = ?")
        .bind(account_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// 标记账号健康状态。
pub async fn mark_health(
    pool: &SqlitePool,
    account_id: i64,
    pool_status: &str,
    health_status: &str,
    reason: &str,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE email_accounts SET pool_status = ?, health_status = ?, health_reason = ?, \
         last_check_at = ? WHERE id = ?",
    )
    .bind(pool_status)
    .bind(health_status)
    .bind(crate::util::truncate_chars(reason, 120))
    .bind(now_naive())
    .bind(account_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// 恢复账号为可用（死号回收）。
pub async fn revive_account(pool: &SqlitePool, account_id: i64) -> AppResult<()> {
    sqlx::query(
        "UPDATE email_accounts SET pool_status = 'available', status = 'available', \
         health_status = 'unknown', health_reason = '', fail_count = 0, claimed_at = NULL \
         WHERE id = ?",
    )
    .bind(account_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// 写回账号凭证（token 轮换后必须落库，否则下次刷新用旧 rt 会失效）。
pub async fn update_account_json(
    pool: &SqlitePool,
    account_id: i64,
    account_json: &str,
) -> AppResult<()> {
    sqlx::query("UPDATE email_accounts SET account_json = ? WHERE id = ?")
        .bind(account_json)
        .bind(account_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ────────────────────────────────────────────────────────────────
//  收件箱基线
// ────────────────────────────────────────────────────────────────

/// 保存（或覆盖）某邮箱的收件箱基线。
///
/// `message_ids` 是逗号分隔的 id 列表。取号瞬间调用，用于挡掉回收号里的旧信。
pub async fn save_baseline(
    pool: &SqlitePool,
    email: &str,
    project_code: &str,
    baseline_at: chrono::NaiveDateTime,
    message_ids: &str,
    captured: bool,
) -> AppResult<()> {
    let key = email.trim().to_ascii_lowercase();
    sqlx::query(
        "INSERT INTO mail_baselines (email, project_code, baseline_at, message_ids, \
         latest_received_at, captured) VALUES (?, ?, ?, ?, NULL, ?) \
         ON CONFLICT(email) DO UPDATE SET baseline_at = excluded.baseline_at, \
         message_ids = excluded.message_ids, captured = excluded.captured, \
         project_code = excluded.project_code",
    )
    .bind(&key)
    .bind(project_code)
    .bind(baseline_at)
    .bind(crate::util::truncate_chars(message_ids, 65536))
    .bind(if captured { 1 } else { 0 })
    .execute(pool)
    .await?;
    Ok(())
}

/// 取某邮箱的基线。
pub async fn get_baseline(pool: &SqlitePool, email: &str) -> AppResult<Option<MailBaseline>> {
    Ok(sqlx::query_as::<_, MailBaseline>(
        "SELECT * FROM mail_baselines WHERE email = ?",
    )
    .bind(email.trim().to_ascii_lowercase())
    .fetch_optional(pool)
    .await?)
}

// ────────────────────────────────────────────────────────────────
//  批量任务统计
// ────────────────────────────────────────────────────────────────

/// 批量任务结果。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BatchResult {
    pub total: i64,
    pub success: i64,
    pub failed: i64,
    pub skipped: i64,
    pub checked: i64,
    pub healthy: i64,
    pub unhealthy: i64,
    pub duration_secs: f64,
}

/// 心跳保活：模拟真人看信（拉收件箱），防微软回收。
///
/// 按「未验证最久的先验」排序，保证每个号都能轮转覆盖，而不是永远只扫
/// id 最小的那批。
pub async fn heartbeat(
    pool_ref: &SqlitePool,
    http: &reqwest::Client,
    mode: &str,
    limit: i64,
) -> AppResult<BatchResult> {
    let started = std::time::Instant::now();
    let accounts = sqlx::query_as::<_, EmailAccount>(
        "SELECT * FROM email_accounts WHERE provider = 'outlook' \
         AND pool_status IN ('available','using') \
         ORDER BY last_check_at IS NULL DESC, last_check_at ASC, id ASC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool_ref)
    .await?;

    let mut r = BatchResult {
        total: accounts.len() as i64,
        ..Default::default()
    };

    for acc in accounts {
        let mut cred = match crate::providers::outlook::Credentials::from_json(&acc.account_json) {
            Ok(c) => c,
            Err(_) => {
                r.failed += 1;
                continue;
            }
        };
        cred.email = acc.email.clone();
        if !acc.proxy.trim().is_empty() {
            cred.proxy = acc.proxy.clone();
        }
        if !cred.is_complete() {
            r.failed += 1;
            continue;
        }

        // interactive 模式多拉几封 + 标记已读（更接近真人）；
        // silent 只刷新 token + 列 1 封（够触发活跃，不打扰）
        let top = if mode == "interactive" { 5 } else { 1 };
        match crate::providers::mailbox::fetch_messages(&mut cred, &acc.email, "inbox", top).await {
            Ok(_) => {
                r.success += 1;
                let _ = sqlx::query(
                    "UPDATE email_accounts SET last_check_at = ?, account_json = ? WHERE id = ?",
                )
                .bind(now_naive())
                .bind(cred.to_json())
                .bind(acc.id)
                .execute(pool_ref)
                .await;
            }
            Err(e) => {
                r.failed += 1;
                tracing::debug!("[heartbeat] {} 失败: {e}", acc.email);
            }
        }
    }

    r.duration_secs = started.elapsed().as_secs_f64();
    let _ = http;
    Ok(r)
}

/// 健康扫描：验证账号是否还能用。
///
/// 7 天缓存：非 force 模式下跳过最近检测过的（否则 995 个号每天全扫一遍，
/// 会打爆微软限流且毫无必要）。
pub async fn check_accounts_health(
    pool_ref: &SqlitePool,
    http: &reqwest::Client,
    provider: &str,
    limit: i64,
    force: bool,
) -> AppResult<BatchResult> {
    let started = std::time::Instant::now();
    let cache_cutoff = now_naive() - chrono::Duration::days(7);

    let candidates = sqlx::query_as::<_, EmailAccount>(
        "SELECT * FROM email_accounts WHERE provider = ? AND pool_status = 'available' \
         AND health_status IN ('unknown','healthy','unhealthy') \
         ORDER BY last_check_at IS NULL DESC, last_check_at ASC, id ASC LIMIT ?",
    )
    .bind(provider)
    // 候选窗口放大：7 天缓存会跳过大量刚检过的号
    .bind(limit * 4)
    .fetch_all(pool_ref)
    .await?;

    let mut to_check: Vec<EmailAccount> = Vec::new();
    let mut skipped = 0i64;
    for acc in candidates {
        if !force {
            if let Some(last) = acc.last_check_at {
                if last > cache_cutoff {
                    skipped += 1;
                    continue;
                }
            }
        }
        to_check.push(acc);
        if to_check.len() as i64 >= limit {
            break;
        }
    }

    tracing::info!(
        "[health-scan] 候选 {} 个，待检测 {} 个，7天缓存跳过 {} 个",
        to_check.len() + skipped as usize,
        to_check.len(),
        skipped
    );

    let mut r = BatchResult {
        total: to_check.len() as i64,
        skipped,
        ..Default::default()
    };

    // 连续风控自动暂停：同一批里连续 5 个被风控，说明整批号都被打标了，
    // 继续扫只是白白触发更多风控
    let mut consecutive_risk = 0u32;
    const MAX_CONSECUTIVE_RISK: u32 = 5;

    for acc in to_check {
        if consecutive_risk >= MAX_CONSECUTIVE_RISK {
            tracing::warn!("[health-scan] 连续 {MAX_CONSECUTIVE_RISK} 个风控，暂停本轮扫描");
            break;
        }
        r.checked += 1;

        let mut cred = match crate::providers::outlook::Credentials::from_json(&acc.account_json) {
            Ok(c) => c,
            Err(_) => {
                r.unhealthy += 1;
                consecutive_risk = 0;
                continue;
            }
        };
        cred.email = acc.email.clone();
        if !acc.proxy.trim().is_empty() {
            cred.proxy = acc.proxy.clone();
        }

        let (ok, detail) = crate::providers::imap::verify_account(&mut cred, &acc.email).await;
        let risk = crate::providers::outlook::is_risk_control(&detail);

        if ok {
            r.healthy += 1;
            consecutive_risk = 0;
            let _ = sqlx::query(
                "UPDATE email_accounts SET health_status = 'healthy', health_reason = '', \
                 fail_count = 0, last_check_at = ?, account_json = ? WHERE id = ?",
            )
            .bind(now_naive())
            .bind(cred.to_json())
            .bind(acc.id)
            .execute(pool_ref)
            .await;
        } else {
            r.unhealthy += 1;
            if risk {
                consecutive_risk += 1;
            } else {
                consecutive_risk = 0;
            }
            // 风控 → dead（这种号不可恢复）；其它 → unhealthy（可能是临时故障）
            let (pool_status, health_status) = if risk {
                ("retired", "dead")
            } else {
                ("available", "unhealthy")
            };
            let _ = sqlx::query(
                "UPDATE email_accounts SET pool_status = ?, health_status = ?, \
                 health_reason = ?, fail_count = fail_count + 1, last_check_at = ? WHERE id = ?",
            )
            .bind(pool_status)
            .bind(health_status)
            .bind(crate::util::truncate_chars(&detail, 120))
            .bind(now_naive())
            .bind(acc.id)
            .execute(pool_ref)
            .await;
        }
    }

    r.duration_secs = started.elapsed().as_secs_f64();
    let _ = http;
    Ok(r)
}

/// 全量刷新 token（提前刷新，防过期）。
pub async fn refresh_all_tokens(
    pool_ref: &SqlitePool,
    http: &reqwest::Client,
    provider: &str,
) -> AppResult<BatchResult> {
    let started = std::time::Instant::now();
    let accounts = sqlx::query_as::<_, EmailAccount>(
        "SELECT * FROM email_accounts WHERE provider = ? AND pool_status <> 'retired' \
         AND account_json LIKE '%refresh_token%'",
    )
    .bind(provider)
    .fetch_all(pool_ref)
    .await?;

    let mut r = BatchResult {
        total: accounts.len() as i64,
        ..Default::default()
    };

    for acc in accounts {
        let mut cred = match crate::providers::outlook::Credentials::from_json(&acc.account_json) {
            Ok(c) => c,
            Err(_) => {
                r.failed += 1;
                continue;
            }
        };
        cred.email = acc.email.clone();
        if !acc.proxy.trim().is_empty() {
            cred.proxy = acc.proxy.clone();
        }

        match crate::providers::outlook::refresh_token(
            http,
            &mut cred,
            crate::providers::outlook::GRAPH_SCOPE,
        )
        .await
        {
            Ok(_) => {
                r.success += 1;
                let _ = sqlx::query(
                    "UPDATE email_accounts SET account_json = ?, last_check_at = ? WHERE id = ?",
                )
                .bind(cred.to_json())
                .bind(now_naive())
                .bind(acc.id)
                .execute(pool_ref)
                .await;
            }
            Err(e) => {
                r.failed += 1;
                tracing::debug!("[refresh-all] {} 失败: {e}", acc.email);
            }
        }
    }

    r.duration_secs = started.elapsed().as_secs_f64();
    Ok(r)
}

/// 所有 tag（去重）。
pub async fn list_tags(pool: &SqlitePool, provider: &str) -> AppResult<Vec<String>> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT tags FROM email_accounts WHERE provider = ? AND tags <> ''",
    )
    .bind(provider)
    .fetch_all(pool)
    .await?;

    let mut set: HashSet<String> = HashSet::new();
    for row in rows {
        for t in row.split([',', ';', ' ']) {
            let t = t.trim();
            if !t.is_empty() {
                set.insert(t.to_string());
            }
        }
    }
    let mut v: Vec<String> = set.into_iter().collect();
    v.sort();
    Ok(v)
}

/// 按 client_id 统计（页面展示用）。
pub async fn stats_by_client_id(
    pool: &SqlitePool,
    provider: &str,
) -> AppResult<Vec<serde_json::Value>> {
    let rows: Vec<(String, String, Option<chrono::NaiveDateTime>)> = sqlx::query_as(
        "SELECT account_json, pool_status, last_check_at FROM email_accounts WHERE provider = ?",
    )
    .bind(provider)
    .fetch_all(pool)
    .await?;

    let mut buckets: std::collections::HashMap<String, (i64, i64)> =
        std::collections::HashMap::new();
    for (json, status, _) in rows {
        let cid = serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .and_then(|v| v.get("client_id").and_then(|c| c.as_str()).map(String::from))
            .unwrap_or_else(|| "(none)".to_string());
        let e = buckets.entry(cid).or_insert((0, 0));
        e.0 += 1;
        if status == POOL_AVAILABLE {
            e.1 += 1;
        }
    }

    let mut v: Vec<_> = buckets
        .into_iter()
        .map(|(cid, (total, avail))| {
            serde_json::json!({"client_id": cid, "total": total, "available": avail})
        })
        .collect();
    v.sort_by(|a, b| {
        b["total"]
            .as_i64()
            .unwrap_or(0)
            .cmp(&a["total"].as_i64().unwrap_or(0))
    });
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bulk_count_matches_single_count() {
        // 验证批量版与单项目版 count_claimable 逐项目结果完全一致。
        // 两个函数必须同源：页面显示用 bulk，实际 claim 用 single，
        // 口径不一致就会出现「显示有货实际取不到」。
        let dir = std::env::temp_dir().join(format!("mp_bulk_{}", crate::util::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::connect(&dir.join("t.db")).await.unwrap();
        crate::db::init_db(&pool).await.unwrap();

        // 造数据：10 个号池号，3 个项目
        for i in 0..10 {
            sqlx::query(
                "INSERT INTO email_accounts (email, provider, account_json, status, pool_status, \
                 health_status, created_at) VALUES (?, 'outlook', '{}', 'available', 'available', 'healthy', ?)",
            )
            .bind(format!("u{i}@t.test"))
            .bind(now_naive())
            .execute(&pool)
            .await
            .unwrap();
        }
        for code in ["p1", "p2", "p3"] {
            sqlx::query(
                "INSERT INTO projects (code, name, provider, config_json, enabled, created_at) \
                 VALUES (?, ?, 'outlook', '{}', 1, ?)",
            )
            .bind(code)
            .bind(code)
            .bind(now_naive())
            .execute(&pool)
            .await
            .unwrap();
        }
        // p1 用掉 2 个号
        for i in 0..2 {
            sqlx::query(
                "INSERT INTO email_usages (email, project_code, first_used_at) VALUES (?, 'p1', ?)",
            )
            .bind(format!("u{i}@t.test"))
            .bind(now_naive())
            .execute(&pool)
            .await
            .unwrap();
        }

        let bulk = count_claimable_bulk(&pool, "outlook").await.unwrap();
        for code in ["p1", "p2", "p3"] {
            let single = count_claimable(&pool, "outlook", code).await.unwrap();
            assert_eq!(
                bulk.get(code).copied().unwrap_or(-1),
                single,
                "项目 {code} 的批量计数与单项目计数不一致"
            );
        }
        assert_eq!(bulk["p1"], 8); // 10 - 2
        assert_eq!(bulk["p2"], 10);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn claim_is_atomic_under_concurrency() {
        let dir = std::env::temp_dir().join(format!("mp_claim_{}", crate::util::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::connect(&dir.join("t.db")).await.unwrap();
        crate::db::init_db(&pool).await.unwrap();

        for i in 0..5 {
            sqlx::query(
                "INSERT INTO email_accounts (email, provider, account_json, status, pool_status, \
                 health_status, created_at) VALUES (?, 'outlook', '{}', 'available', 'available', 'healthy', ?)",
            )
            .bind(format!("c{i}@t.test"))
            .bind(now_naive())
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO projects (code, name, provider, config_json, enabled, created_at) \
             VALUES ('px', 'px', 'outlook', '{}', 1, ?)",
        )
        .bind(now_naive())
        .execute(&pool)
        .await
        .unwrap();

        // 20 个并发各取一个号，池里只有 5 个 → 必须恰好 5 个成功、且邮箱互不重复
        let mut handles = Vec::new();
        for _ in 0..20 {
            let p = pool.clone();
            handles.push(tokio::spawn(async move {
                claim_account(&p, "outlook", "px", |acc| async move {
                    let _ = acc;
                    (true, String::new())
                })
                .await
                .unwrap()
            }));
        }
        let mut got: Vec<String> = Vec::new();
        for h in handles {
            if let Some(c) = h.await.unwrap() {
                got.push(c.email);
            }
        }
        got.sort();
        got.dedup();
        assert_eq!(got.len(), 5, "并发下必须恰好取走 5 个不重复的号，实际 {got:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ────────────────────────────────────────────────────────────
    //  count_claimable_bulk 回归（修掉的「减法」bug）
    // ────────────────────────────────────────────────────────────

    /// 建一个最小可用的内存库（accounts + usages + projects）。


    /// 测试用：插一个项目（补齐 NOT NULL 列）。
    async fn seed_project(pool: &sqlx::SqlitePool, code: &str) {
        sqlx::query(
            "INSERT INTO projects (code, name, provider, config_json, enabled, created_at) \
             VALUES (?, ?, 'outlook', '{}', 1, ?)",
        )
        .bind(code)
        .bind(code)
        .bind(now_naive())
        .execute(pool)
        .await
        .unwrap();
    }

    /// 测试用：插一个可 claim 的账号。
    ///
    /// 完整 schema 里 email_accounts 有多列 NOT NULL，
    /// 直接 INSERT 很容易漏（provider / created_at…）。
    async fn seed_account(pool: &sqlx::SqlitePool, email: &str) {
        seed_account_with(pool, email, "healthy").await;
    }

    async fn seed_account_with(pool: &sqlx::SqlitePool, email: &str, health: &str) {
        sqlx::query(
            "INSERT INTO email_accounts \
             (email, provider, account_json, status, pool_status, health_status, created_at) \
             VALUES (?, 'outlook', '{}', 'available', 'available', ?, ?)",
        )
        .bind(email)
        .bind(health)
        .bind(now_naive())
        .execute(pool)
        .await
        .unwrap();
    }

    /// 建一个内存库。
    ///
    /// 用 `db::init_db`（完整 schema）而不是手写最小表 —— 手写表会漏掉
    /// 后续新增的列（比如独占邮箱加的 `exclusive_owner`），
    /// 导致 SQL 引用新列时测试全挂，而生产是对的，白费排查时间。
    async fn setup_stock_db() -> sqlx::SqlitePool {
        use std::str::FromStr;
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
        crate::db::init_db(&pool).await.unwrap();
        pool
    }

    /// **核心回归：用过的邮箱离开池子后，库存不能被多扣。**
    ///
    /// 这正是生产上 chatgpt001 显示 0（实际 55）的原因：
    /// 旧算法 `base - used_total`，其中 used_total 包含了已经不在池子里的号。
    #[tokio::test]
    async fn stock_not_over_subtracted_when_used_accounts_left_pool() {
        let pool = setup_stock_db().await;

        // 池子里 5 个可用号：a b c d e
        for e in ["a@x.test", "b@x.test", "c@x.test", "d@x.test", "e@x.test"] {
            seed_account(&pool, e).await;
        }
        // 外加 3 个**已不在池子里**的号（health=dead）
        for e in ["f@x.test", "g@x.test", "h@x.test"] {
            seed_account_with(&pool, e, "dead").await;
        }

        seed_project(&pool, "p1").await;

        // p1 用过 a、b（还在池子里）和 f、g、h（已经死了）
        for e in ["a@x.test", "b@x.test", "f@x.test", "g@x.test", "h@x.test"] {
            sqlx::query("INSERT INTO email_usages (email, project_code, first_used_at) VALUES (?, 'p1', ?)")
                .bind(e)
                .bind(now_naive())
                .execute(&pool)
                .await
                .unwrap();
        }

        let m = count_claimable_bulk(&pool, "outlook").await.unwrap();
        let got = m.get("p1").copied().unwrap_or(-1);

        // 正确：池子 5 个里，a、b 被 p1 用过 → 剩 c、d、e = 3
        assert_eq!(got, 3, "真实可 claim 应为 3（c/d/e）");

        // 旧错误算法会算成：base(5) - used_total(5) = 0 ← 明显错
        assert_ne!(got, 0, "不能被已离开池子的号多扣（这就是生产上的 bug）");
    }

    /// 全新项目：池子里的号全都能用。
    #[tokio::test]
    async fn stock_full_for_unused_project() {
        let pool = setup_stock_db().await;
        for e in ["a@x.test", "b@x.test", "c@x.test"] {
            seed_account(&pool, e).await;
        }
        seed_project(&pool, "fresh").await;

        let m = count_claimable_bulk(&pool, "outlook").await.unwrap();
        assert_eq!(m.get("fresh").copied().unwrap(), 3);
    }

    /// 多个项目互相独立：p1 用过 a，p2 仍能用 a。
    #[tokio::test]
    async fn stock_is_per_project_independent() {
        let pool = setup_stock_db().await;
        for e in ["a@x.test", "b@x.test"] {
            seed_account(&pool, e).await;
        }
        for c in ["p1", "p2"] {
            seed_project(&pool, c).await;
        }
        sqlx::query("INSERT INTO email_usages (email, project_code, first_used_at) VALUES ('a@x.test', 'p1', ?)")
            .bind(now_naive())
            .execute(&pool)
            .await
            .unwrap();

        let m = count_claimable_bulk(&pool, "outlook").await.unwrap();
        assert_eq!(m.get("p1").copied().unwrap(), 1, "p1 只剩 b");
        assert_eq!(m.get("p2").copied().unwrap(), 2, "p2 两个都能用");
    }

    /// 库存永不为负（旧算法在 chatgpt001 上算出 -12）。
    #[tokio::test]
    async fn stock_never_negative() {
        let pool = setup_stock_db().await;
        seed_account(&pool, "only@x.test").await;
        seed_project(&pool, "hungry").await;
        // 用过 10 个（其中 9 个是已消失的号）
        for i in 0..10 {
            sqlx::query("INSERT INTO email_usages (email, project_code, first_used_at) VALUES (?, 'hungry', ?)")
                .bind(format!("gone{i}@x.test"))
                .bind(now_naive())
                .execute(&pool)
                .await
                .unwrap();
        }

        let m = count_claimable_bulk(&pool, "outlook").await.unwrap();
        assert_eq!(m.get("hungry").copied().unwrap(), 1, "只剩唯一那个可用号");
    }

    /// 死了的号不算库存。
    #[tokio::test]
    async fn stock_excludes_dead_accounts() {
        let pool = setup_stock_db().await;
        seed_account(&pool, "ok@x.test").await;
        seed_account_with(&pool, "dead@x.test", "dead").await;
        seed_project(&pool, "p").await;

        let m = count_claimable_bulk(&pool, "outlook").await.unwrap();
        assert_eq!(m.get("p").copied().unwrap(), 1, "只算 healthy 的");
    }

    /// 停用的项目不出现在结果里。
    #[tokio::test]
    async fn disabled_projects_excluded() {
        let pool = setup_stock_db().await;
        seed_account(&pool, "a@x.test").await;
        seed_project(&pool, "on").await;
        sqlx::query(
            "INSERT INTO projects (code, name, provider, config_json, enabled, created_at) \
             VALUES ('off', 'off', 'outlook', '{}', 0, ?)",
        )
        .bind(now_naive())
        .execute(&pool)
        .await
        .unwrap();

        let m = count_claimable_bulk(&pool, "outlook").await.unwrap();
        assert!(m.contains_key("on"));
        assert!(!m.contains_key("off"), "停用项目不该出现");
    }

}
