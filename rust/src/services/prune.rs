//! 自动剔除「完全不能用」的号。
//!
//! ## 什么算「完全不能用」
//!
//! 生产实测的失效形态（2026-11-23）：
//!
//! | health_reason | 数量 | 含义 | 能否恢复 |
//! |---|---|---|---|
//! | `abuse_mode` | 131 | 微软 Service Abuse Mode 封禁 | **不能** |
//! | `risk_control` | 35 | 风控锁定（AADSTS70000） | **不能** |
//! | `provider_fatal` | 1 | 凭据彻底失效（invalid_grant） | **不能** |
//!
//! 这些号刷新 token 会被永久拒绝，永远收不到信，留在池子里只是
//! 拖慢候选查询、污染统计。
//!
//! 反例（**不能删**）：
//! - `health_status = 'unhealthy'` —— 只是这次检测失败，可能是网络抖动
//! - `health_status = 'unknown'` —— 还没检测过
//! - 正在 `using` 的号 —— 用户还在等验证码
//! - 被别人独占持有的号 —— 是别人的资产
//! - 有 pending 订单的号 —— 删了这个号，冻结的钱就对不上账
//!
//! ## 为什么要「失效满 N 天」
//!
//! 健康检查偶尔误判（代理挂了会被当成号死了）。给一个冷静期，
//! 期间如果重新检测变 healthy，就自动不会被选中。默认 7 天。
//!
//! ## 为什么留 prune_log
//!
//! 剔除是**破坏性操作**，删掉的号连同它的 refresh_token 一起没了。
//! 万一规则写宽了，得能查「什么时候删了哪些号、为什么删」。
//! 日志只存邮箱和原因，不存凭据（凭据是敏感数据，不留副本）。

use chrono::{Duration, NaiveDateTime};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};
use crate::util::now_naive;

/// 剔除配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruneConfig {
    /// 是否启用自动剔除
    pub enabled: bool,
    /// 失效满多少天才剔除（冷静期，防误判）
    pub days: i64,
    /// 每日执行时刻（时）
    pub hour: u32,
    /// 每日执行时刻（分）
    pub minute: u32,
    /// 演练模式：只统计不真删
    pub dry_run: bool,
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self {
            enabled: false, // 默认关闭：破坏性操作必须显式开启
            days: 7,
            hour: 4,
            minute: 30,
            dry_run: true, // 默认演练：先让人看清会删什么
        }
    }
}

const CFG_KEY: &str = "pool.prune";

pub async fn load_config(pool: &SqlitePool) -> AppResult<PruneConfig> {
    let raw = crate::services::settings::get_value(pool, CFG_KEY, "{}").await?;
    let mut cfg: PruneConfig = serde_json::from_str(&raw).unwrap_or_default();
    // 兜底：天数不能为负/0，否则等于「刚死就删」
    if cfg.days < 1 {
        cfg.days = 1;
    }
    if cfg.hour > 23 {
        cfg.hour = 4;
    }
    if cfg.minute > 59 {
        cfg.minute = 30;
    }
    Ok(cfg)
}

pub async fn save_config(pool: &SqlitePool, patch: &serde_json::Value) -> AppResult<PruneConfig> {
    let mut cfg = load_config(pool).await?;
    if let Some(v) = patch.get("enabled").and_then(|v| v.as_bool()) {
        cfg.enabled = v;
    }
    if let Some(v) = patch.get("days").and_then(|v| v.as_i64()) {
        cfg.days = v.clamp(1, 365);
    }
    if let Some(v) = patch.get("hour").and_then(|v| v.as_i64()) {
        cfg.hour = (v.clamp(0, 23)) as u32;
    }
    if let Some(v) = patch.get("minute").and_then(|v| v.as_i64()) {
        cfg.minute = (v.clamp(0, 59)) as u32;
    }
    if let Some(v) = patch.get("dry_run").and_then(|v| v.as_bool()) {
        cfg.dry_run = v;
    }
    let raw = serde_json::to_string(&cfg)?;
    crate::services::settings::set_value(pool, CFG_KEY, &raw).await?;
    Ok(cfg)
}

/// 一个待剔除的号。
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PruneCandidate {
    pub id: i64,
    pub email: String,
    pub health_status: String,
    pub health_reason: String,
    pub fail_count: i64,
    pub last_check_at: Option<NaiveDateTime>,
    /// 距今失效天数
    pub age_days: i64,
}

/// 候选筛选条件（预览和真删共用，保证「看到的就是会删的」）。
///
/// 参数占位顺序：days, days
const CANDIDATE_WHERE: &str = r#"
    a.health_status = 'dead'
    AND a.pool_status != 'using'
    AND COALESCE(a.exclusive_owner, 0) = 0
    AND NOT EXISTS (
        SELECT 1 FROM exclusive_emails e
        WHERE e.email = a.email AND e.status = 'active'
    )
    AND NOT EXISTS (
        SELECT 1 FROM email_orders o
        WHERE o.email = a.email AND o.billing_status = 'pending'
    )
    AND COALESCE(a.last_check_at, a.created_at) < ?
    AND a.created_at < ?
"#;

/// 预览：列出会被剔除的号（不删任何东西）。
pub async fn preview(pool: &SqlitePool, days: i64) -> AppResult<Vec<PruneCandidate>> {
    let cutoff = now_naive() - Duration::days(days.max(1));
    let rows = sqlx::query_as::<_, PruneCandidate>(&format!(
        "SELECT a.id, a.email, a.health_status,
                COALESCE(a.health_reason, '') AS health_reason,
                a.fail_count, a.last_check_at,
                CAST(julianday('now') - julianday(COALESCE(a.last_check_at, a.created_at)) AS INTEGER) AS age_days
         FROM email_accounts a
         WHERE {CANDIDATE_WHERE}
         ORDER BY a.last_check_at ASC"
    ))
    .bind(cutoff)
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// 剔除统计。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PruneReport {
    /// 本次删掉（或演练中「将删」）的数量
    pub removed: i64,
    /// 是否演练（true = 没真删）
    pub dry_run: bool,
    /// 按原因分组
    pub by_reason: Vec<(String, i64)>,
    /// 耗时毫秒
    pub duration_ms: u64,
    /// 执行时间
    pub ran_at: String,
}

/// 执行剔除。
pub async fn run(pool: &SqlitePool, cfg: &PruneConfig) -> AppResult<PruneReport> {
    let started = std::time::Instant::now();
    let candidates = preview(pool, cfg.days).await?;

    // 按原因分组（两种模式都要统计，便于看趋势）
    let mut by_reason: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for c in &candidates {
        let k = if c.health_reason.is_empty() {
            "(未记录原因)".to_string()
        } else {
            c.health_reason.clone()
        };
        *by_reason.entry(k).or_insert(0) += 1;
    }
    let mut by_reason: Vec<(String, i64)> = by_reason.into_iter().collect();
    by_reason.sort_by(|a, b| b.1.cmp(&a.1));

    let ran_at = crate::util::utc_now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    if candidates.is_empty() {
        return Ok(PruneReport {
            removed: 0,
            dry_run: cfg.dry_run,
            by_reason,
            duration_ms: started.elapsed().as_millis() as u64,
            ran_at,
        });
    }

    if cfg.dry_run {
        tracing::info!(
            "[prune] 演练模式：将剔除 {} 个失效号（未实际删除）",
            candidates.len()
        );
        return Ok(PruneReport {
            removed: candidates.len() as i64,
            dry_run: true,
            by_reason,
            duration_ms: started.elapsed().as_millis() as u64,
            ran_at,
        });
    }

    // 真删：逐个删（不用 IN 批量，方便精确记录每个号的原因）
    let mut removed = 0i64;
    for c in &candidates {
        // 再确认一次状态没变（预览到执行之间可能被健康检查救活）
        let still_dead: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM email_accounts \
             WHERE id = ? AND health_status = 'dead' AND pool_status != 'using'",
        )
        .bind(c.id)
        .fetch_one(pool)
        .await?;
        if still_dead == 0 {
            continue;
        }

        // 先写审计日志（万一删除失败，日志里也有痕迹）
        let _ = sqlx::query(
            "INSERT INTO prune_log \
             (email, health_reason, health_status, fail_count, age_days, pruned_at, dry_run) \
             VALUES (?, ?, ?, ?, ?, ?, 0)",
        )
        .bind(&c.email)
        .bind(&c.health_reason)
        .bind(&c.health_status)
        .bind(c.fail_count)
        .bind(c.age_days)
        .bind(now_naive())
        .execute(pool)
        .await;

        let res = sqlx::query("DELETE FROM email_accounts WHERE id = ? AND health_status = 'dead'")
            .bind(c.id)
            .execute(pool)
            .await?;
        if res.rows_affected() > 0 {
            removed += 1;
        }
    }

    tracing::info!(
        "[prune] 已剔除 {} 个失效号（候选 {} 个）",
        removed,
        candidates.len()
    );

    Ok(PruneReport {
        removed,
        dry_run: false,
        by_reason,
        duration_ms: started.elapsed().as_millis() as u64,
        ran_at,
    })
}

/// 剔除日志的一条。
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PruneLogRow {
    pub id: i64,
    pub email: String,
    pub health_reason: String,
    pub health_status: String,
    pub fail_count: i64,
    pub age_days: i64,
    pub pruned_at: NaiveDateTime,
    pub dry_run: i64,
}

/// 最近的剔除记录。
pub async fn history(pool: &SqlitePool, limit: i64) -> AppResult<Vec<PruneLogRow>> {
    Ok(sqlx::query_as::<_, PruneLogRow>(
        "SELECT id, email, health_reason, health_status, fail_count, age_days, pruned_at, dry_run \
         FROM prune_log ORDER BY id DESC LIMIT ?",
    )
    .bind(limit.clamp(1, 500))
    .fetch_all(pool)
    .await?)
}

/// 建表（幂等）。
pub async fn ensure_schema(pool: &SqlitePool) -> AppResult<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS prune_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            email VARCHAR(254) NOT NULL,
            health_reason VARCHAR(128) NOT NULL DEFAULT '',
            health_status VARCHAR(16) NOT NULL DEFAULT '',
            fail_count INTEGER NOT NULL DEFAULT 0,
            age_days INTEGER NOT NULL DEFAULT 0,
            pruned_at DATETIME NOT NULL,
            dry_run INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS ix_prune_log_time ON prune_log (pruned_at)")
        .execute(pool)
        .await?;
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

    async fn seed(pool: &SqlitePool, email: &str, health: &str, reason: &str, days_ago: i64) {
        let t = now_naive() - Duration::days(days_ago);
        sqlx::query(
            "INSERT INTO email_accounts \
             (email, provider, account_json, status, pool_status, health_status, health_reason, \
              fail_count, last_check_at, created_at) \
             VALUES (?, 'outlook', '{}', 'available', 'available', ?, ?, 3, ?, ?)",
        )
        .bind(email)
        .bind(health)
        .bind(reason)
        .bind(t)
        .bind(t)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn prunes_dead_accounts_after_cooldown() {
        let pool = setup().await;
        seed(&pool, "dead-old@x.test", "dead", "abuse_mode", 10).await;
        seed(&pool, "healthy@x.test", "healthy", "", 10).await;

        let c = preview(&pool, 7).await.unwrap();
        assert_eq!(c.len(), 1, "只该选中失效满 7 天的");
        assert_eq!(c[0].email, "dead-old@x.test");

        let cfg = PruneConfig { days: 7, dry_run: false, ..Default::default() };
        let rep = run(&pool, &cfg).await.unwrap();
        assert_eq!(rep.removed, 1);

        // 健康号必须还在
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM email_accounts WHERE email='healthy@x.test'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(n, 1);
        // 日志有记录
        let logs = history(&pool, 10).await.unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].health_reason, "abuse_mode");
    }

    #[tokio::test]
    async fn respects_cooldown_period() {
        let pool = setup().await;
        // 刚死 1 天，不该删
        seed(&pool, "dead-new@x.test", "dead", "abuse_mode", 1).await;
        let c = preview(&pool, 7).await.unwrap();
        assert!(c.is_empty(), "冷静期内不能删（防健康检查误判）");
    }

    #[tokio::test]
    async fn never_prunes_unhealthy_or_unknown() {
        let pool = setup().await;
        // unhealthy = 可能只是网络抖动；unknown = 没测过
        seed(&pool, "unhealthy@x.test", "unhealthy", "", 30).await;
        seed(&pool, "unknown@x.test", "unknown", "", 30).await;
        let c = preview(&pool, 7).await.unwrap();
        assert!(c.is_empty(), "只删 dead，不碰 unhealthy/unknown");
    }

    #[tokio::test]
    async fn skips_accounts_in_use_or_exclusive() {
        let pool = setup().await;
        // 正在占用
        sqlx::query(
            "INSERT INTO email_accounts \
             (email, provider, account_json, status, pool_status, health_status, health_reason, last_check_at, created_at) \
             VALUES ('using@x.test','outlook','{}','using','using','dead','abuse_mode',?,?)",
        )
        .bind(now_naive() - Duration::days(30))
        .bind(now_naive() - Duration::days(30))
        .execute(&pool).await.unwrap();

        // 被独占
        sqlx::query(
            "INSERT INTO email_accounts \
             (email, provider, account_json, status, pool_status, health_status, health_reason, exclusive_owner, last_check_at, created_at) \
             VALUES ('excl@x.test','outlook','{}','available','exclusive','dead','abuse_mode',7,?,?)",
        )
        .bind(now_naive() - Duration::days(30))
        .bind(now_naive() - Duration::days(30))
        .execute(&pool).await.unwrap();

        let c = preview(&pool, 7).await.unwrap();
        assert!(c.is_empty(), "占用中/被独占的不能删");
    }

    #[tokio::test]
    async fn skips_accounts_with_pending_orders() {
        let pool = setup().await;
        seed(&pool, "billed@x.test", "dead", "abuse_mode", 30).await;
        sqlx::query(
            "INSERT INTO email_orders (user_id, email, project_code, provider, price, billing_status, \
             hold_amount, charged_amount, created_at) \
             VALUES (1, 'billed@x.test', 'p1', 'outlook', 100, 'pending', 100, 0, ?)",
        )
        .bind(now_naive())
        .execute(&pool).await.unwrap();

        let c = preview(&pool, 7).await.unwrap();
        assert!(c.is_empty(), "有 pending 订单的不能删（钱还没结清）");
    }

    #[tokio::test]
    async fn dry_run_does_not_delete() {
        let pool = setup().await;
        seed(&pool, "dead@x.test", "dead", "risk_control", 10).await;

        let cfg = PruneConfig { days: 7, dry_run: true, ..Default::default() };
        let rep = run(&pool, &cfg).await.unwrap();
        assert!(rep.dry_run);
        assert_eq!(rep.removed, 1, "演练报告应显示「将删 1 个」");

        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM email_accounts")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(n, 1, "演练模式绝不能真删");
    }

    #[tokio::test]
    async fn groups_by_reason() {
        let pool = setup().await;
        seed(&pool, "a@x.test", "dead", "abuse_mode", 10).await;
        seed(&pool, "b@x.test", "dead", "abuse_mode", 12).await;
        seed(&pool, "c@x.test", "dead", "risk_control", 15).await;

        let cfg = PruneConfig { days: 7, dry_run: true, ..Default::default() };
        let rep = run(&pool, &cfg).await.unwrap();
        assert_eq!(rep.removed, 3);
        assert_eq!(rep.by_reason[0], ("abuse_mode".to_string(), 2));
    }
}
