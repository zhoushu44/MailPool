//! 邮件服务：项目 CRUD、取号编排、邮件落库、收件箱摘要。
//!
//! 与 Python `app/services/mail_service.py` 对齐。取号流程：
//! `claim（原子占位+预检）→ bind（按项目一次性）→ snapshot baseline（防旧码）`

use chrono::NaiveDateTime;
use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};
use crate::models::*;
use crate::services::pool;
use crate::util::{now_naive, truncate_chars};

// ────────────────────────────────────────────────────────────────
//  项目
// ────────────────────────────────────────────────────────────────

pub async fn get_project(pool: &SqlitePool, code: &str) -> AppResult<Option<Project>> {
    Ok(sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE code = ?")
        .bind(code)
        .fetch_optional(pool)
        .await?)
}

/// 取项目（不存在或已停用都算「没有」）。
pub async fn get_enabled_project(pool: &SqlitePool, code: &str) -> AppResult<Option<Project>> {
    Ok(
        sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE code = ? AND enabled = 1")
            .bind(code)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn list_projects(pool: &SqlitePool) -> AppResult<Vec<Project>> {
    Ok(
        sqlx::query_as::<_, Project>("SELECT * FROM projects ORDER BY id ASC")
            .fetch_all(pool)
            .await?,
    )
}

pub async fn list_enabled_projects(pool: &SqlitePool) -> AppResult<Vec<Project>> {
    Ok(
        sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE enabled = 1 ORDER BY id ASC")
            .fetch_all(pool)
            .await?,
    )
}

#[allow(clippy::too_many_arguments)]
pub async fn upsert_project(
    pool: &SqlitePool,
    project_id: Option<i64>,
    code: &str,
    name: &str,
    provider: &str,
    config_json: &str,
    enabled: bool,
) -> AppResult<i64> {
    let enabled_i = if enabled { 1 } else { 0 };
    if let Some(id) = project_id {
        let res = sqlx::query(
            "UPDATE projects SET code = ?, name = ?, provider = ?, config_json = ?, enabled = ? \
             WHERE id = ?",
        )
        .bind(code)
        .bind(name)
        .bind(provider)
        .bind(config_json)
        .bind(enabled_i)
        .bind(id)
        .execute(pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(AppError::not_found("项目不存在"));
        }
        return Ok(id);
    }

    // 新建：先查 code 是否重复（给友好错误，而不是裸 UNIQUE 约束异常）
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projects WHERE code = ?")
        .bind(code)
        .fetch_one(pool)
        .await?;
    if exists > 0 {
        return Err(AppError::bad_request(format!("项目 code 已存在: {code}")));
    }
    let res = sqlx::query(
        "INSERT INTO projects (code, name, provider, config_json, enabled, created_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(code)
    .bind(name)
    .bind(provider)
    .bind(config_json)
    .bind(enabled_i)
    .bind(now_naive())
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn delete_project(pool: &SqlitePool, project_id: i64) -> AppResult<bool> {
    let res = sqlx::query("DELETE FROM projects WHERE id = ?")
        .bind(project_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

// ────────────────────────────────────────────────────────────────
//  取号
// ────────────────────────────────────────────────────────────────

/// 取号结果（对应 Python `acquire_email` 的返回 dict）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AcquiredEmail {
    pub email: String,
    pub provider: String,
    pub project_code: String,
    pub project_name: String,
    pub account_id: Option<i64>,
    pub status: String,
    pub acquired_at: String,
}

/// 按 `project_code` 取一个邮箱。
///
/// 固定源（outlook）：从号池 claim（跳过**本项目**已用过的邮箱），claim 后绑定。
/// 若绑定冲突（并发兜底）则释放该号并返回 no_stock，由调用方重试。
pub async fn acquire_email<F, Fut>(
    pool_ref: &SqlitePool,
    project_code: &str,
    probe: F,
) -> AppResult<AcquiredEmail>
where
    F: FnMut(EmailAccount) -> Fut + Clone,
    Fut: std::future::Future<Output = (bool, String)>,
{
    let project = get_enabled_project(pool_ref, project_code)
        .await?
        .ok_or_else(|| AppError::bad_request(format!("未知项目 code: {project_code}")))?;

    // 当前只有 outlook 一个 provider，全部走号池分支
    let claimed = pool::claim_account(pool_ref, &project.provider, &project.code, probe)
        .await?
        .ok_or_else(|| AppError::bad_request("邮箱池无可用账号（本项目用过的邮箱已排除）"))?;

    // claim 后再校验一次绑定（并发兜底），冲突则跳过该号
    let (ok, other) = pool::bind_email(pool_ref, &claimed.email, &project.code).await?;
    if !ok {
        pool::release_account(pool_ref, claimed.account_id).await?;
        return Err(AppError::bad_request(format!(
            "邮箱 {} 已被项目 {} 使用过，已跳过该号",
            claimed.email,
            other.unwrap_or_default()
        )));
    }

    Ok(AcquiredEmail {
        email: claimed.email,
        provider: project.provider,
        project_code: project.code,
        project_name: project.name,
        account_id: Some(claimed.account_id),
        status: "holding".to_string(),
        acquired_at: crate::util::utc_now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
    })
}

// ────────────────────────────────────────────────────────────────
//  邮件落库
// ────────────────────────────────────────────────────────────────

/// 保存一条收件记录（同一 `email + message_id` 幂等）。
pub async fn save_mail_record(
    pool: &SqlitePool,
    email: &str,
    project_code: &str,
    message_id: &str,
    provider: &str,
    folder: &str,
    sender: &str,
    subject: &str,
    body_preview: &str,
    body: &str,
    code: &str,
    code_source: &str,
    received_at: Option<NaiveDateTime>,
) -> AppResult<MailRecord> {
    let normalized_email = email.trim().to_ascii_lowercase();
    let mid = truncate_chars(message_id, 512);

    if !mid.is_empty() {
        if let Some(existing) = sqlx::query_as::<_, MailRecord>(
            "SELECT * FROM mail_records WHERE email = ? AND message_id = ? LIMIT 1",
        )
        .bind(&normalized_email)
        .bind(&mid)
        .fetch_optional(pool)
        .await?
        {
            return Ok(existing);
        }
    }

    let res = sqlx::query(
        "INSERT INTO mail_records (email, project_code, message_id, provider, folder, sender, \
         subject, body_preview, body, code, code_source, received_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&normalized_email)
    .bind(project_code)
    .bind(&mid)
    .bind(truncate_chars(provider, 32))
    .bind(truncate_chars(folder, 64))
    .bind(truncate_chars(sender, 254))
    .bind(truncate_chars(subject, 512))
    .bind(body_preview)
    .bind(body)
    .bind(truncate_chars(code, 32))
    .bind(truncate_chars(code_source, 16))
    .bind(received_at.unwrap_or_else(now_naive))
    .execute(pool)
    .await?;

    Ok(sqlx::query_as::<_, MailRecord>("SELECT * FROM mail_records WHERE id = ?")
        .bind(res.last_insert_rowid())
        .fetch_one(pool)
        .await?)
}

/// 邮件记录列表（按收件时间倒序）。
pub async fn list_mail_records(
    pool: &SqlitePool,
    email: Option<&str>,
    project_code: Option<&str>,
    emails: Option<&[String]>,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<MailRecord>> {
    let mut sql =
        String::from("SELECT * FROM mail_records WHERE 1=1");
    if emails.is_some() {
        sql.push_str(&format!(
            " AND email IN ({})",
            std::iter::repeat("?").take(emails.map(|e| e.len()).unwrap_or(0)).collect::<Vec<_>>().join(",")
        ));
    } else if let Some(e) = email.filter(|s| !s.is_empty()) {
        sql.push_str(&format!(" AND email = '{}'", e.to_ascii_lowercase().replace('\'', "''")));
    }
    if let Some(pc) = project_code.filter(|s| !s.is_empty()) {
        sql.push_str(&format!(
            " AND project_code = '{}'",
            pc.replace('\'', "''")
        ));
    }
    sql.push_str(" ORDER BY received_at DESC LIMIT ? OFFSET ?");

    let mut q = sqlx::query_as::<_, MailRecord>(&sql);
    if let Some(list) = emails {
        for e in list {
            q = q.bind(e);
        }
    }
    let rows = q.bind(limit).bind(offset).fetch_all(pool).await?;
    Ok(rows)
}

pub async fn count_mail_records(pool: &SqlitePool) -> AppResult<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM mail_records")
        .fetch_one(pool)
        .await?)
}

/// 按邮箱集合计数（客户费用明细用）。
///
/// 为什么要单独一个函数：客户只能看自己取过的邮箱的收件记录，
/// 直接 `count_mail_records` 会统计全表（越权）。
pub async fn count_mail_records_by_emails(
    pool: &SqlitePool,
    emails: &[String],
) -> AppResult<i64> {
    if emails.is_empty() {
        return Ok(0);
    }
    let placeholders = vec!["?"; emails.len()].join(",");
    let sql = format!(
        "SELECT COUNT(*) FROM mail_records WHERE email IN ({placeholders})"
    );
    let mut q = sqlx::query_scalar::<_, i64>(&sql);
    for e in emails {
        q = q.bind(e);
    }
    Ok(q.fetch_one(pool).await?)
}

/// 今天收到的邮件数 / 其中带验证码的数量。
pub async fn today_stats(pool: &SqlitePool) -> AppResult<(i64, i64)> {
    // 按 UTC 当天（对齐 Python `datetime.now(timezone.utc).date()`）
    let today = crate::util::utc_now().date_naive();
    let start = today.and_hms_opt(0, 0, 0).unwrap();
    let end = start + chrono::Duration::days(1);
    let mails: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mail_records WHERE received_at >= ? AND received_at < ?",
    )
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await?;
    let codes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mail_records WHERE received_at >= ? AND received_at < ? \
         AND code <> ''",
    )
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await?;
    Ok((mails, codes))
}

// ────────────────────────────────────────────────────────────────
//  收件箱摘要（按邮箱聚合，仿 MailNest /emails/temporary）
// ────────────────────────────────────────────────────────────────

/// 一个邮箱的汇总行。
#[derive(Debug, Clone, serde::Serialize)]
pub struct EmailSummary {
    pub email: String,
    pub project_code: String,
    pub project_name: String,
    pub pooled: bool,
    pub account_status: String,
    pub first_used_at: String,
    pub last_received_at: String,
    pub code: String,
    pub mail_count: i64,
}

/// 按邮箱聚合出「临时邮箱列表」（每行一个邮箱地址）。
pub async fn list_email_summaries(
    pool: &SqlitePool,
    project_code: Option<&str>,
    limit: i64,
    offset: i64,
) -> AppResult<(Vec<EmailSummary>, i64)> {
    let (total, usages) = if let Some(pc) = project_code.filter(|s| !s.is_empty()) {
        let total: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM email_usages WHERE project_code = ?")
                .bind(pc)
                .fetch_one(pool)
                .await?;
        let rows = sqlx::query_as::<_, EmailUsage>(
            "SELECT * FROM email_usages WHERE project_code = ? \
             ORDER BY first_used_at DESC LIMIT ? OFFSET ?",
        )
        .bind(pc)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
        (total, rows)
    } else {
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM email_usages")
            .fetch_one(pool)
            .await?;
        let rows = sqlx::query_as::<_, EmailUsage>(
            "SELECT * FROM email_usages ORDER BY first_used_at DESC LIMIT ? OFFSET ?",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
        (total, rows)
    };

    if usages.is_empty() {
        return Ok((Vec::new(), total));
    }

    let emails: Vec<String> = usages.iter().map(|u| u.email.to_ascii_lowercase()).collect();

    // 项目名
    let projects = list_projects(pool).await?;
    let proj_names: std::collections::HashMap<String, String> = projects
        .into_iter()
        .map(|p| {
            let label = if p.name.is_empty() {
                p.code.clone()
            } else {
                p.name.clone()
            };
            (p.code, label)
        })
        .collect();

    // 号池账号状态
    let placeholders = std::iter::repeat("?")
        .take(emails.len())
        .collect::<Vec<_>>()
        .join(",");
    let acc_sql = format!(
        "SELECT email, pool_status FROM email_accounts WHERE email IN ({placeholders})"
    );
    let mut aq = sqlx::query_as::<_, (String, String)>(&acc_sql);
    for e in &emails {
        aq = aq.bind(e);
    }
    let acc_map: std::collections::HashMap<String, String> = aq
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|(e, s)| (e.to_ascii_lowercase(), s))
        .collect();

    // 每个邮箱最新一封邮件（窗口函数按邮箱分组取最新）
    let mail_sql = format!(
        "SELECT email, received_at, code FROM ( \
           SELECT email, received_at, code, \
                  ROW_NUMBER() OVER (PARTITION BY email ORDER BY received_at DESC) AS rn \
           FROM mail_records WHERE email IN ({placeholders}) \
         ) WHERE rn = 1"
    );
    let mut mq = sqlx::query_as::<_, (String, NaiveDateTime, String)>(&mail_sql);
    for e in &emails {
        mq = mq.bind(e);
    }
    let latest: std::collections::HashMap<String, (NaiveDateTime, String)> = mq
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|(e, at, code)| (e.to_ascii_lowercase(), (at, code)))
        .collect();

    // 每个邮箱的邮件总数
    let count_sql = format!(
        "SELECT email, COUNT(*) FROM mail_records WHERE email IN ({placeholders}) GROUP BY email"
    );
    let mut cq = sqlx::query_as::<_, (String, i64)>(&count_sql);
    for e in &emails {
        cq = cq.bind(e);
    }
    let counts: std::collections::HashMap<String, i64> = cq
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|(e, n)| (e.to_ascii_lowercase(), n))
        .collect();

    let rows = usages
        .into_iter()
        .map(|u| {
            let key = u.email.to_ascii_lowercase();
            let (last_at, code) = match latest.get(&key) {
                Some((at, c)) => (Some(*at), c.clone()),
                None => (None, String::new()),
            };
            EmailSummary {
                email: u.email.clone(),
                project_code: u.project_code.clone(),
                project_name: proj_names
                    .get(&u.project_code)
                    .cloned()
                    .unwrap_or_else(|| u.project_code.clone()),
                pooled: acc_map.contains_key(&key),
                account_status: acc_map.get(&key).cloned().unwrap_or_default(),
                first_used_at: crate::util::iso_from_naive(Some(u.first_used_at)),
                last_received_at: crate::util::iso_from_naive(last_at),
                code,
                mail_count: counts.get(&key).copied().unwrap_or(0),
            }
        })
        .collect();

    Ok((rows, total))
}
