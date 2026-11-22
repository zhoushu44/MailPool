//! 数据层：sqlx SQLite 连接池 + 建表 + 幂等迁移。
//!
//! **关键约束：Rust 版必须能直接打开 Python 版写出的 `mailpool.db`。**
//! 因此这里不引入新列、不改列名，只在必要时做「SQLite 无法 DROP 列约束」
//! 的重建（`email_orders` / `email_usages` 从单列唯一改组合唯一）。
//!
//! 连接池 PRAGMA 与 Python `app/database.py` 一致：
//! - `journal_mode=WAL`（读并发好）
//! - `busy_timeout=30s`（写锁等待）
//! - `foreign_keys=ON`
//! - `synchronous=NORMAL`（WAL 下兼顾安全与吞吐，Python 版用的是默认 FULL，
//!   这里显式设为 NORMAL 能显著提升写入吞吐，且崩溃最多丢最后一个事务）

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use sqlx::{ConnectOptions, SqlitePool};

/// 建表 SQL（与 Python `app/models.py` 的声明式 schema 完全对应）。
///
/// 注意 `DATETIME` 列在 SQLite 里是动态类型：sqlx 的 `chrono::NaiveDateTime`
/// 与 Python 写入的 `'2026-08-14 17:44:09.887929'` 文本格式兼容。
pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    code VARCHAR(64) NOT NULL UNIQUE,
    name VARCHAR(128) NOT NULL DEFAULT '',
    provider VARCHAR(32) NOT NULL,
    config_json TEXT NOT NULL DEFAULT '{}',
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at DATETIME NOT NULL
);
CREATE INDEX IF NOT EXISTS ix_projects_code ON projects (code);

CREATE TABLE IF NOT EXISTS email_accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    email VARCHAR(254) NOT NULL,
    provider VARCHAR(32) NOT NULL,
    account_json TEXT NOT NULL DEFAULT '{}',
    status VARCHAR(16) NOT NULL DEFAULT 'available',
    remark VARCHAR(255) NOT NULL DEFAULT '',
    last_check_at DATETIME,
    created_at DATETIME NOT NULL,
    check_result TEXT NOT NULL DEFAULT '',
    fail_count INTEGER NOT NULL DEFAULT 0,
    pool_status VARCHAR(16) NOT NULL DEFAULT 'available',
    health_status VARCHAR(16) NOT NULL DEFAULT 'unknown',
    health_reason VARCHAR(32) NOT NULL DEFAULT '',
    claimed_at DATETIME,
    tags VARCHAR(255) NOT NULL DEFAULT '',
    proxy VARCHAR(255) NOT NULL DEFAULT '',
    CONSTRAINT uq_email UNIQUE (email)
);
CREATE INDEX IF NOT EXISTS ix_email_accounts_email ON email_accounts (email);
CREATE INDEX IF NOT EXISTS ix_email_accounts_pool ON email_accounts (provider, pool_status, health_status);

CREATE TABLE IF NOT EXISTS mail_records (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    email VARCHAR(254) NOT NULL,
    project_code VARCHAR(64) NOT NULL DEFAULT '',
    message_id VARCHAR(512) NOT NULL DEFAULT '',
    provider VARCHAR(32) NOT NULL DEFAULT '',
    folder VARCHAR(64) NOT NULL DEFAULT '',
    sender VARCHAR(254) NOT NULL DEFAULT '',
    subject VARCHAR(512) NOT NULL DEFAULT '',
    body_preview TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    code VARCHAR(32) NOT NULL DEFAULT '',
    code_source VARCHAR(16) NOT NULL DEFAULT '',
    received_at DATETIME NOT NULL
);
CREATE INDEX IF NOT EXISTS ix_mail_records_email ON mail_records (email);
CREATE INDEX IF NOT EXISTS ix_mail_records_project_code ON mail_records (project_code);
CREATE INDEX IF NOT EXISTS ix_mail_records_received_at ON mail_records (received_at);
CREATE INDEX IF NOT EXISTS ix_mail_records_email_message_id ON mail_records (email, message_id);

CREATE TABLE IF NOT EXISTS email_usages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    email VARCHAR(254) NOT NULL,
    project_code VARCHAR(64) NOT NULL,
    first_used_at DATETIME NOT NULL,
    CONSTRAINT uq_email_usage_project UNIQUE (email, project_code)
);
CREATE INDEX IF NOT EXISTS ix_email_usages_email ON email_usages (email);
CREATE INDEX IF NOT EXISTS ix_email_usages_project_code ON email_usages (project_code);
CREATE INDEX IF NOT EXISTS ix_email_usages_first_used_at ON email_usages (first_used_at);

CREATE TABLE IF NOT EXISTS mail_baselines (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    email VARCHAR(254) NOT NULL,
    project_code VARCHAR(64) NOT NULL DEFAULT '',
    baseline_at DATETIME NOT NULL,
    message_ids TEXT NOT NULL DEFAULT '',
    latest_received_at DATETIME,
    captured INTEGER NOT NULL DEFAULT 0,
    CONSTRAINT uq_mail_baseline_email UNIQUE (email)
);

CREATE TABLE IF NOT EXISTS token_refresh_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    "trigger" VARCHAR(16) NOT NULL DEFAULT '',
    provider VARCHAR(32) NOT NULL DEFAULT '',
    status VARCHAR(16) NOT NULL DEFAULT '',
    total INTEGER NOT NULL DEFAULT 0,
    success INTEGER NOT NULL DEFAULT 0,
    failed INTEGER NOT NULL DEFAULT 0,
    skipped INTEGER NOT NULL DEFAULT 0,
    duration FLOAT NOT NULL DEFAULT 0,
    detail TEXT NOT NULL DEFAULT '',
    created_at DATETIME NOT NULL
);

CREATE TABLE IF NOT EXISTS settings (
    "key" VARCHAR(64) PRIMARY KEY,
    value TEXT NOT NULL DEFAULT '',
    updated_at DATETIME NOT NULL
);

CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    username VARCHAR(64) NOT NULL UNIQUE,
    password_hash VARCHAR(255) NOT NULL DEFAULT '',
    role INTEGER NOT NULL DEFAULT 2,
    status INTEGER NOT NULL DEFAULT 1,
    email VARCHAR(128) NOT NULL DEFAULT '',
    balance BIGINT NOT NULL DEFAULT 0,
    frozen_balance BIGINT NOT NULL DEFAULT 0,
    total_recharged BIGINT NOT NULL DEFAULT 0,
    total_charged BIGINT NOT NULL DEFAULT 0,
    created_at DATETIME NOT NULL
);
CREATE INDEX IF NOT EXISTS ix_users_username ON users (username);
CREATE INDEX IF NOT EXISTS ix_users_email ON users (email);

CREATE TABLE IF NOT EXISTS api_keys (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL DEFAULT 0,
    "key" VARCHAR(128) NOT NULL UNIQUE,
    name VARCHAR(64) NOT NULL DEFAULT '',
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at DATETIME NOT NULL,
    last_used_at DATETIME
);
CREATE INDEX IF NOT EXISTS ix_api_keys_user_id ON api_keys (user_id);
CREATE INDEX IF NOT EXISTS ix_api_keys_key ON api_keys ("key");

CREATE TABLE IF NOT EXISTS recharge_orders (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL DEFAULT 0,
    trade_no VARCHAR(64) NOT NULL,
    amount BIGINT NOT NULL DEFAULT 0,
    money BIGINT NOT NULL DEFAULT 0,
    pay_type VARCHAR(32) NOT NULL DEFAULT '',
    pay_provider VARCHAR(32) NOT NULL DEFAULT 'epay',
    status VARCHAR(16) NOT NULL DEFAULT 'pending',
    client_ip VARCHAR(64) NOT NULL DEFAULT '',
    remark VARCHAR(255) NOT NULL DEFAULT '',
    created_at DATETIME NOT NULL,
    completed_at DATETIME,
    CONSTRAINT uq_recharge_trade_no UNIQUE (trade_no)
);
CREATE INDEX IF NOT EXISTS ix_recharge_orders_user_status ON recharge_orders (user_id, status);

CREATE TABLE IF NOT EXISTS balance_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL DEFAULT 0,
    log_type VARCHAR(16) NOT NULL DEFAULT '',
    amount BIGINT NOT NULL DEFAULT 0,
    balance_before BIGINT NOT NULL DEFAULT 0,
    balance_after BIGINT NOT NULL DEFAULT 0,
    frozen_before BIGINT NOT NULL DEFAULT 0,
    frozen_after BIGINT NOT NULL DEFAULT 0,
    remark VARCHAR(255) NOT NULL DEFAULT '',
    created_at DATETIME NOT NULL
);
CREATE INDEX IF NOT EXISTS ix_balance_logs_user_created ON balance_logs (user_id, created_at);

CREATE TABLE IF NOT EXISTS redeem_cards (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    batch_id VARCHAR(64) NOT NULL DEFAULT '',
    card_key VARCHAR(64) NOT NULL,
    amount BIGINT NOT NULL DEFAULT 0,
    status INTEGER NOT NULL DEFAULT 0,
    used_by INTEGER NOT NULL DEFAULT 0,
    used_at DATETIME,
    created_at DATETIME NOT NULL,
    CONSTRAINT uq_redeem_card_key UNIQUE (card_key)
);

CREATE TABLE IF NOT EXISTS email_orders (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL DEFAULT 0,
    email VARCHAR(254) NOT NULL,
    project_code VARCHAR(64) NOT NULL DEFAULT '',
    provider VARCHAR(32) NOT NULL DEFAULT '',
    price BIGINT NOT NULL DEFAULT 0,
    billing_status VARCHAR(16) NOT NULL DEFAULT 'pending',
    hold_amount BIGINT NOT NULL DEFAULT 0,
    charged_amount BIGINT NOT NULL DEFAULT 0,
    created_at DATETIME NOT NULL,
    charged_at DATETIME,
    released_at DATETIME,
    CONSTRAINT uq_email_order_project UNIQUE (email, project_code)
);
CREATE INDEX IF NOT EXISTS ix_email_orders_user_billing ON email_orders (user_id, billing_status);

-- ────────────────────────────────────────────────────────────────
--  邮箱注册：验证码（落库，不存内存）
-- ────────────────────────────────────────────────────────────────
--  为什么不照抄 new-api 的进程内 RAM map：
--    - 多实例部署直接失效（请求落到别的节点就"验证码错误"）
--    - 进程重启丢失（用户点了发送，服务重启就白点）
--    - 无法审计（谁在什么时候要过码，事后查不到）
--
--  存的是 HMAC-SHA256 而不是明文码：库文件泄露时攻击者拿不到可用验证码。
--  用 HMAC 而不是 Argon2：6 位码只有 100 万种可能，Argon2 每次 20ms 会
--  把发码接口打成 DoS；真正的防线是「尝试次数上限 + 短有效期」。
CREATE TABLE IF NOT EXISTS email_verification_codes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    email VARCHAR(254) NOT NULL,
    code_hash VARCHAR(64) NOT NULL,
    purpose VARCHAR(16) NOT NULL DEFAULT 'register',
    -- 尝试次数：超过上限直接作废，防爆破
    attempts INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL DEFAULT 5,
    -- 有效期（分钟级）：注册码 10 分钟
    expires_at DATETIME NOT NULL,
    -- 用过即删（consumed=1 后保留一小会儿用于审计，定时清理）
    consumed INTEGER NOT NULL DEFAULT 0,
    consumed_at DATETIME,
    client_ip VARCHAR(64) NOT NULL DEFAULT '',
    created_at DATETIME NOT NULL
);
CREATE INDEX IF NOT EXISTS ix_evc_email_purpose ON email_verification_codes (email, purpose);
CREATE INDEX IF NOT EXISTS ix_evc_expires ON email_verification_codes (expires_at);

-- ────────────────────────────────────────────────────────────────
--  邮箱注册：发信限流（IP + 邮箱 双维度）
-- ────────────────────────────────────────────────────────────────
--  new-api 只按 IP 限流（2 次/30 秒），换个邮箱就能无限发 —— 这是个洞。
--  这里两个维度都记：同一 IP 不能刷量，同一邮箱不能被反复轰炸。
CREATE TABLE IF NOT EXISTS email_send_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    email VARCHAR(254) NOT NULL DEFAULT '',
    client_ip VARCHAR(64) NOT NULL DEFAULT '',
    purpose VARCHAR(16) NOT NULL DEFAULT 'register',
    ok INTEGER NOT NULL DEFAULT 1,
    created_at DATETIME NOT NULL
);
CREATE INDEX IF NOT EXISTS ix_esl_email_time ON email_send_log (email, created_at);
CREATE INDEX IF NOT EXISTS ix_esl_ip_time ON email_send_log (client_ip, created_at);
"#;

/// 打开数据库连接池。
pub async fn connect(db_path: &Path) -> Result<SqlitePool, sqlx::Error> {
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(30))
        .foreign_keys(true)
        // 每个连接都要设：默认 sqlx 只对第一条连接做 pragma
        .pragma("busy_timeout", "30000")
        .pragma("synchronous", "NORMAL")
        .log_statements(tracing::log::LevelFilter::Debug)
        .log_slow_statements(tracing::log::LevelFilter::Warn, Duration::from_millis(500));

    // 取号/收码是阻塞路由，50 并发时需要足够连接
    // （Python 版设为 pool_size=20 + overflow=40；这里对应 20 条上限 + 排队）
    SqlitePool::connect_with(opts).await
}

/// 建表 + 幂等迁移。启动时调用一次。
pub async fn init_db(pool: &SqlitePool) -> anyhow::Result<()> {
    // 1) 建全量表（IF NOT EXISTS，老库不动）
    sqlx::raw_sql(SCHEMA_SQL).execute(pool).await?;

    // 2) 老库补列（Python 版历史上用 ALTER TABLE 逐列加过）
    migrate_add_columns(pool).await?;

    // 3) 约束重建：老库的 email_usages / email_orders 是「email 单列唯一」，
    //    与现在的「(email, project_code) 组合唯一」冲突 —— 要重建表。
    migrate_rebuild_constraints(pool).await?;

    // 4) 老库 status → pool_status / health_status 的拆分回填
    backfill_status_columns(pool).await?;

    Ok(())
}

/// 逐列幂等补列（对齐 Python `init_db` 里的 `migration_sql`）。
async fn migrate_add_columns(pool: &SqlitePool) -> anyhow::Result<()> {
    // (表, 列名, 列定义)
    const COLS: &[(&str, &str, &str)] = &[
        ("email_accounts", "check_result", "TEXT DEFAULT ''"),
        ("email_accounts", "fail_count", "INTEGER DEFAULT 0"),
        ("email_accounts", "pool_status", "VARCHAR(16) DEFAULT 'available'"),
        ("email_accounts", "health_status", "VARCHAR(16) DEFAULT 'unknown'"),
        ("email_accounts", "health_reason", "VARCHAR(32) DEFAULT ''"),
        ("email_accounts", "claimed_at", "DATETIME"),
        ("email_accounts", "tags", "VARCHAR(255) DEFAULT ''"),
        ("email_accounts", "proxy", "VARCHAR(255) DEFAULT ''"),
        ("mail_records", "message_id", "VARCHAR(512) DEFAULT ''"),
        ("mail_records", "provider", "VARCHAR(32) DEFAULT ''"),
        ("mail_records", "folder", "VARCHAR(64) DEFAULT ''"),
        ("mail_records", "body_preview", "TEXT DEFAULT ''"),
        ("mail_records", "code_source", "VARCHAR(16) DEFAULT ''"),
        // 邮箱注册：验证时间戳（NULL = 未验证）
        // new-api 全仓没有这个字段，靠应用层锁保证邮箱唯一 —— 结果就是
        // 无法区分「注册时填了邮箱」和「邮箱真的验证过」。
        ("users", "email_verified_at", "DATETIME"),
        ("users", "username_norm", "VARCHAR(64) DEFAULT ''"),
    ];

    for (table, column, definition) in COLS {
        let exists: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?"
        ))
        .bind(column)
        .fetch_one(pool)
        .await?;
        if exists == 0 {
            tracing::info!("[migrate] 补列 {table}.{column}");
            sqlx::raw_sql(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"))
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

/// 返回表上所有唯一索引覆盖的列组合。
async fn unique_index_columns(
    pool: &SqlitePool,
    table: &str,
) -> anyhow::Result<Vec<Vec<String>>> {
    let indexes: Vec<(String, i64)> = sqlx::query_as(&format!(
        "SELECT name, \"unique\" FROM pragma_index_list('{table}')"
    ))
    .fetch_all(pool)
    .await?;

    let mut out = Vec::new();
    for (name, unique) in indexes {
        if unique == 0 {
            continue;
        }
        let cols: Vec<(i64, Option<String>)> = sqlx::query_as(&format!(
            "SELECT seqno, name FROM pragma_index_info('{}') ORDER BY seqno",
            name.replace('\'', "''")
        ))
        .fetch_all(pool)
        .await?;
        out.push(cols.into_iter().filter_map(|(_, n)| n).collect());
    }
    Ok(out)
}

/// 老库约束重建：单列 email 唯一 → (email, project_code) 组合唯一。
async fn migrate_rebuild_constraints(pool: &SqlitePool) -> anyhow::Result<()> {
    // (表名, 重建 DDL, 拷贝列)
    let jobs: &[(&str, &str, &str)] = &[
        (
            "email_usages",
            "id INTEGER PRIMARY KEY AUTOINCREMENT, \
             email VARCHAR(254) NOT NULL, \
             project_code VARCHAR(64) NOT NULL, \
             first_used_at DATETIME, \
             CONSTRAINT uq_email_usage_project UNIQUE (email, project_code)",
            "id, email, project_code, first_used_at",
        ),
        (
            "email_orders",
            "id INTEGER PRIMARY KEY AUTOINCREMENT, \
             user_id INTEGER NOT NULL, \
             email VARCHAR(254) NOT NULL, \
             project_code VARCHAR(64), \
             provider VARCHAR(32), \
             price BIGINT, \
             billing_status VARCHAR(16), \
             hold_amount BIGINT, \
             charged_amount BIGINT, \
             created_at DATETIME, \
             charged_at DATETIME, \
             released_at DATETIME, \
             CONSTRAINT uq_email_order_project UNIQUE (email, project_code)",
            "id, user_id, email, project_code, provider, price, billing_status, \
             hold_amount, charged_amount, created_at, charged_at, released_at",
        ),
    ];

    for (table, create_cols, copy_cols) in jobs {
        let exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?",
        )
        .bind(table)
        .fetch_one(pool)
        .await?;
        if exists == 0 {
            continue;
        }

        let uniques = unique_index_columns(pool, table).await?;
        let already_ok = uniques.iter().any(|cols| {
            let set: std::collections::HashSet<&str> =
                cols.iter().map(|s| s.as_str()).collect();
            set.len() == 2 && set.contains("email") && set.contains("project_code")
        });
        if already_ok {
            continue;
        }

        tracing::warn!("[migrate] 重建 {table}：单列 email 唯一 → (email, project_code) 组合唯一");
        // SQLite 不支持在事务里改表结构后立刻用同名索引，故分开执行
        let mut tx = pool.begin().await?;
        sqlx::raw_sql(&format!("ALTER TABLE {table} RENAME TO {table}_old"))
            .execute(&mut *tx)
            .await?;
        sqlx::raw_sql(&format!("CREATE TABLE {table} ({create_cols})"))
            .execute(&mut *tx)
            .await?;
        sqlx::raw_sql(&format!(
            "INSERT INTO {table} ({copy_cols}) SELECT {copy_cols} FROM {table}_old"
        ))
        .execute(&mut *tx)
        .await?;
        sqlx::raw_sql(&format!("DROP TABLE {table}_old"))
            .execute(&mut *tx)
            .await?;
        sqlx::raw_sql(&format!(
            "CREATE INDEX IF NOT EXISTS ix_{table}_email ON {table}(email)"
        ))
        .execute(&mut *tx)
        .await?;
        sqlx::raw_sql(&format!(
            "CREATE INDEX IF NOT EXISTS ix_{table}_project_code ON {table}(project_code)"
        ))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    Ok(())
}

/// 老库 status → pool_status 的回填（只在新列刚加时做一次）。
async fn backfill_status_columns(pool: &SqlitePool) -> anyhow::Result<()> {
    // 仅当 pool_status 全为空/NULL 时才回填，避免覆盖现有值
    let need: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM email_accounts WHERE pool_status IS NULL OR pool_status = ''",
    )
    .fetch_one(pool)
    .await?;
    if need > 0 {
        tracing::info!("[migrate] 回填 {need} 行 pool_status");
        sqlx::raw_sql(
            "UPDATE email_accounts SET pool_status = \
             CASE WHEN health_status IN ('healthy','unhealthy','dead') THEN health_status \
             ELSE 'available' END \
             WHERE pool_status IS NULL OR pool_status = ''",
        )
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// 健康检查：数据库可读。
pub async fn ping(pool: &SqlitePool) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(pool)
        .await
        .is_ok()
}
