//! 客户收件页：`/app/mailbox`
//!
//! ## 为什么需要这个
//!
//! 客户在「购买邮箱」点了「获取邮箱」后，要能看到这个邮箱收到的邮件、
//! 拿到验证码。原来购买成功后跳 `/app/mailbox`，但这个路由根本不存在
//! —— 直接 404。这是复刻 MailNest 时漏掉的一整块。
//!
//! ## 权限模型
//!
//! 客户只能看**自己取过号的**邮箱。判定依据是 `email_usages`：
//! 该邮箱被当前用户用某个项目取过（`user_id` 匹配）。
//!
//! 不能只凭「知道邮箱地址」就允许读信 —— 邮箱是全局共享池，
//! 别人正在用的号也可能被猜到。必须校验归属。
//!
//! ## 与管理端收件箱的区别
//!
//! | | 管理端 `/mailbox` | 客户 `/app/mailbox` |
//! |---|---|---|
//! | 邮箱列表 | 全部 995 个 | 只有自己取过的 |
//! | 能看到 | 所有邮件、原始头、AI 提取 | 邮件正文 + 验证码 |
//! | 能操作 | 删号、刷新 token、健康检查 | 只看，不能改 |

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{AppState, Identity};
use crate::error::{api_err, ok, AppError, AppResult, CODE_OP_FAILED};

/// 客户收件相关路由。
pub fn router() -> axum::Router<Arc<AppState>> {
    use axum::routing::get;
    axum::Router::new()
        .route("/api/app/mailbox/messages", get(app_messages))
        .route("/api/app/mailbox/message", get(app_message))
        .route("/api/app/mailbox/my-emails", get(my_emails))
}

/// 校验这个邮箱是不是当前客户取过的。
async fn ensure_owns(
    pool: &sqlx::SqlitePool,
    identity: &Identity,
    email: &str,
) -> AppResult<()> {
    // 管理员可以看任何邮箱（方便排查）
    if identity.is_admin {
        return Ok(());
    }
    let Some(uid) = identity.user_id else {
        return Err(AppError::unauthorized("需要登录"));
    };

    // 归属判定：email_usages 里有这个 (email, user_id) 的组合。
    // 老数据可能 user_id=0（未接入计费时期的匿名取号），此时退化为
    // 「只要这个邮箱被取过就放行」—— 与 Python 版的行为一致。
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM email_orders o \
         WHERE lower(o.email) = lower(?) AND (o.user_id = ? OR o.user_id = 0)",
    )
    .bind(email)
    .bind(uid)
    .fetch_one(pool)
    .await?;

    if n > 0 {
        return Ok(());
    }

    // 退回 email_usages（没有订单表的旧数据）
    // 独占邮箱：直接看归属表
    let n3: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM exclusive_emails \
         WHERE lower(email) = lower(?) AND user_id = ? AND status = 'active'",
    )
    .bind(email)
    .bind(uid)
    .fetch_one(pool)
    .await?;
    if n3 > 0 {
        return Ok(());
    }

    // 私有邮箱：客户自己上传的号
    if crate::services::private_email::owns(pool, uid, email).await? {
        return Ok(());
    }

    Err(AppError::unauthorized("这个邮箱不在你的取号记录里"))
}

#[derive(Debug, Deserialize)]
struct MyEmailsQuery {
    #[serde(default)]
    limit: Option<i64>,
}

/// `GET /api/app/mailbox/my-emails` —— 我取过的邮箱（收件页左侧列表）。
async fn my_emails(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Query(q): Query<MyEmailsQuery>,
) -> AppResult<Json<Value>> {
    let Some(uid) = identity.user_id else {
        if identity.is_admin {
            // 管理员没绑客户身份时给空列表（不要报错，页面还要能开）
            return Ok(ok(json!({ "items": [], "total": 0 })));
        }
        return Err(AppError::unauthorized("需要登录"));
    };

    let limit = q.limit.unwrap_or(30).clamp(1, 100);

    // 最近取过的邮箱，按取号时间倒序。
    //
    // 状态直接取自 email_accounts.pool_status（唯一权威来源）。
    //
    // ⚠ 外层 SELECT 必须给子查询的列起**别名并显式引用**：
    // 原来写 `lower(a.email) = lower(email)` 会报
    // `ambiguous column name: email` —— 因为 email_accounts(a)、
    // email_orders(o) 和子查询里都有 email，SQLite 无法判断指哪个。
    let rows: Vec<(String, String, String, Option<chrono::NaiveDateTime>)> = sqlx::query_as(
        // GROUP BY 邮箱去重：同一个号可能既有 order 又有多个项目下的
        // usage 记录，UNION 因 ts 不同去不掉重，界面上会出现同一个
        // 邮箱好几行（实测 zs 的 4 个号显示成 8 行）。
        // SQLite 里 MAX() 聚合时其余列取自最大值那一行，正好是「最近一次」。
        "SELECT sub.mail, sub.proj, COALESCE(a.pool_status, 'unknown') AS st, \
                MAX(sub.ts) AS ts \
         FROM ( \
             SELECT o.email AS mail, o.project_code AS proj, o.created_at AS ts \
             FROM email_orders o WHERE o.user_id = ? \
             UNION \
             SELECT u.email AS mail, u.project_code AS proj, u.first_used_at AS ts \
             FROM email_usages u \
             WHERE NOT EXISTS ( \
                 SELECT 1 FROM email_orders e \
                 WHERE e.email = u.email AND e.project_code = u.project_code AND e.user_id = ? \
             ) \
               AND EXISTS (SELECT 1 FROM email_orders e2 WHERE e2.email = u.email AND e2.user_id = ?) \
             UNION \
             SELECT x.email AS mail, '(独占)' AS proj, x.created_at AS ts \
             FROM exclusive_emails x \
             WHERE x.user_id = ? AND x.status = 'active' \
              UNION \
              SELECT p.email AS mail, '(私有)' AS proj, p.created_at AS ts \
              FROM private_emails p \
              WHERE p.user_id = ? AND p.status = 'active' \
         ) AS sub \
         LEFT JOIN email_accounts a ON lower(a.email) = lower(sub.mail) \
         GROUP BY lower(sub.mail) \
         ORDER BY MAX(sub.ts) DESC LIMIT ?",
    )
    .bind(uid)
    .bind(uid)
    .bind(uid)
    .bind(uid)
    .bind(uid)
    .bind(limit)
    .fetch_all(&st.pool)
    .await?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(|(email, project_code, st, created_at)| {
            let status = match st.as_str() {
                "using" => "holding",
                _ => "released",
            };
            json!({
                "email": email,
                "project_code": project_code,
                "status": status,
                "pool_status": st,
                "created_at": crate::util::iso_from_naive(created_at),
            })
        })
        .collect();

    Ok(ok(json!({
        "total": items.len(),
        "items": items,
    })))
}

#[derive(Debug, Deserialize)]
struct MsgQuery {
    email: String,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    top: Option<i64>,
}

/// `GET /api/app/mailbox/messages` —— 拉某个邮箱的邮件列表。
async fn app_messages(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Query(q): Query<MsgQuery>,
) -> AppResult<Json<Value>> {
    let email = q.email.trim();
    if email.is_empty() {
        return Ok(api_err(CODE_OP_FAILED, "缺少 email 参数"));
    }
    ensure_owns(&st.pool, &identity, email).await?;

    let folder = q.folder.as_deref().unwrap_or("inbox");
    let top = q.top.unwrap_or(15).clamp(1, 50) as u32;

    match crate::api::mailbox::list_messages(&st.pool, email, folder, top).await {
        Ok(mails) => {
            // list_messages 返回的可能是 {items:[...]} 或直接 [...]，
            // 两种都兼容（底层是 Graph 的原始响应形状）
            let arr: Vec<Value> = match &mails {
                Value::Array(a) => a.clone(),
                Value::Object(o) => o
                    .get("items")
                    .or_else(|| o.get("value"))
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default(),
                _ => Vec::new(),
            };

            // 每封邮件跑一遍规则提取，把验证码直接给前端
            let out: Vec<Value> = arr
                .iter()
                .map(|m| {
                    let code = crate::api::mailbox::extract_rule(m);
                    let mut v = m.clone();
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("code".into(), json!(code));
                    }
                    v
                })
                .collect();

            Ok(ok(json!({ "total": out.len(), "items": out })))
        }
        Err(e) => {
            tracing::warn!("[app-mail] 拉取失败 email={email}: {e}");
            Ok(api_err(CODE_OP_FAILED, format!("{e}")))
        }
    }
}

#[derive(Debug, Deserialize)]
struct OneMsgQuery {
    email: String,
    message_id: String,
}

/// `GET /api/app/mailbox/message` —— 单封邮件详情。
async fn app_message(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Query(q): Query<OneMsgQuery>,
) -> AppResult<Json<Value>> {
    let email = q.email.trim();
    ensure_owns(&st.pool, &identity, email).await?;

    match crate::api::mailbox::get_message(&st.pool, email, &q.message_id).await {
        Ok(m) => Ok(ok(m)),
        Err(e) => Ok(api_err(CODE_OP_FAILED, format!("{e}"))),
    }
}
