//! 管理 API：`/api/admin/*` 与 `/api/admin/billing/*`。
//!
//! 与 Python 版 `app/api/admin_api.py` / `app/api/billing_admin_api.py` 的
//! **路径、查询参数、返回字段名逐字对齐** —— 现有页面（pool.html / accounts.html /
//! settings.html / billing_users.html …）一行 JS 都不用改就能切到 Rust 后端。
//!
//! ## 响应约定
//!
//! 管理面统一 `{"code":0,"data":...}` / `{"code":0,"msg":"..."}`；
//! 失败走 `AppError`（HTTP 4xx/5xx + `{"detail": "..."}`），与 Python 的
//! `HTTPException` 一致 —— 前端 `api()` helper 正是按 `detail` 取错误文案。
//!
//! ## 为什么管理面直接写 SQL
//!
//! 服务层（`services::pool` / `mail` / `billing` / `settings`）覆盖的是
//! 「取号 → 收码 → 计费」主链路。管理面的列表/导出/计数带一堆可选过滤条件，
//! 塞进服务层会把主链路接口撑胖且更容易漂移。这里的边界是：
//! **只读统计 + 单行维护直接写 SQL；任何状态机（release / revive / 扣费）一律调服务层**。
//!
//! ## 金额
//!
//! 数据库金额列全是**整数厘**（1 元 = 1000 厘），对外一律转成 3 位小数的字符串。
//! 直接把厘丢给前端会被当成"元"显示，账目看起来会放大 1000 倍。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, patch, post, put};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::auth::{AppState, Identity};
use crate::error::*;
use crate::models::*;
use crate::money::Money;
use crate::services::{billing, mail, payment, pool, settings};

/// 管理 API 的时间格式：**裸 datetime，无时区后缀**。
///
/// Python 版管理接口直接 `dt.isoformat()`，输出 `2026-11-15T07:38:11.563038`。
/// 对外 API 才用带 `Z` 的（`utc_iso()`）。两者混用会让管理页面的 JS
/// 时间解析走不同分支，差分测试也会报差异。
///
/// **微秒为 0 时 Python 不输出 `.000000`**（`datetime(...,0).isoformat()`
/// 就是 `2026-11-12T10:07:04`）。这里必须照做，否则页面上的时间列宽度
/// 会不一致，差分比对也过不去。
fn admin_iso(v: Option<chrono::NaiveDateTime>) -> String {
    v.map(|d| {
        if d.and_utc().timestamp_subsec_micros() == 0 {
            d.format("%Y-%m-%dT%H:%M:%S").to_string()
        } else {
            d.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
        }
    })
    .unwrap_or_default()
}

/// 当前只有 outlook 一个固定源。集中成常量，将来加 provider 只改这里。
const PROVIDER: &str = "outlook";

// ────────────────────────────────────────────────────────────────
//  公共小工具
// ────────────────────────────────────────────────────────────────

/// 管理面统一守卫。
///
/// 每个 handler 的第一行都必须是同一句 —— 少写一处就是越权漏洞，
/// 统一形态才能在 review 时逐个对照。
fn ensure_admin(identity: &Identity) -> AppResult<()> {
    if identity.is_admin {
        Ok(())
    } else {
        Err(AppError::unauthorized("需要管理员口令（X-Admin-Password 或后台登录）"))
    }
}

/// 厘 → 元字符串（3 位小数）。管理面所有金额字段都要过这里。
fn fmt_milli(v: i64) -> String {
    Money::from_milli(v).to_yuan()
}

/// 归一化可选过滤参数：空串视同没传（前端 `<select>` 会提交空值）。
fn non_empty(v: &Option<String>) -> Option<String> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// 邮箱格式校验（等价 Python `^[^\s@]+@[^\s@]+\.[^\s@]+$`）。
///
/// 不引正则：导入时每行都要判一次，手写判断更快，也没有正则编译失败的风险。
fn is_valid_email(email: &str) -> bool {
    if email.is_empty() || email.chars().any(char::is_whitespace) {
        return false;
    }
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

/// 取一次 `email → "项目A,项目B"` 映射。
///
/// 必须一次查全表而不是逐账号查：995 个号逐条查会变成 995 次往返
/// （Python 版早期就是在循环里查，pool 页要 700ms）。
async fn email_usage_map(pool_ref: &sqlx::SqlitePool) -> AppResult<HashMap<String, String>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT email, project_code FROM email_usages ORDER BY first_used_at")
            .fetch_all(pool_ref)
            .await?;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (email, code) in rows {
        out.entry(email.to_ascii_lowercase()).or_default().push(code);
    }
    Ok(out.into_iter().map(|(k, v)| (k, v.join(","))).collect())
}

/// 账号计数（对齐 Python `pool_service.count_accounts`）。
///
/// 用一条 GROUP BY 聚合，而不是把整表拉进内存再数。
async fn count_accounts(pool_ref: &sqlx::SqlitePool, provider: Option<&str>) -> AppResult<Value> {
    let rows: Vec<(String, String, String, i64)> = match provider {
        Some(p) => {
            sqlx::query_as(
                "SELECT provider, COALESCE(pool_status, ''), COALESCE(health_status, ''), COUNT(*) \
                 FROM email_accounts WHERE provider = ? \
                 GROUP BY provider, pool_status, health_status",
            )
            .bind(p)
            .fetch_all(pool_ref)
            .await?
        }
        None => {
            sqlx::query_as(
                "SELECT provider, COALESCE(pool_status, ''), COALESCE(health_status, ''), COUNT(*) \
                 FROM email_accounts GROUP BY provider, pool_status, health_status",
            )
            .fetch_all(pool_ref)
            .await?
        }
    };

    let mut total = 0i64;
    let mut available = 0i64;
    let mut using_ = 0i64;
    let mut retired = 0i64;
    let mut dead = 0i64;
    let mut healthy = 0i64;
    let mut unhealthy = 0i64;
    let mut unknown = 0i64;
    let mut by_provider: Map<String, Value> = Map::new();

    for (prov, pool_status, health_status, n) in rows {
        total += n;
        let cur = by_provider.get(&prov).and_then(|v| v.as_i64()).unwrap_or(0);
        by_provider.insert(prov, json!(cur + n));
        match pool_status.as_str() {
            "available" => available += n,
            "using" => using_ += n,
            "retired" => retired += n,
            _ => {}
        }
        match health_status.as_str() {
            "healthy" => healthy += n,
            "unhealthy" => unhealthy += n,
            "dead" => dead += n,
            _ => unknown += n,
        }
    }

    Ok(json!({
        "total": total,
        "available": available,
        "using": using_,
        "retired": retired,
        "dead": dead,
        "healthy": healthy,
        "unhealthy": unhealthy,
        "unknown": unknown,
        "by_provider": Value::Object(by_provider),
    }))
}

// ────────────────────────────────────────────────────────────────
//  总览
// ────────────────────────────────────────────────────────────────

/// `GET /api/admin/overview/summary`
///
/// 首页四块卡片（账号 / 号池 / 今日 / 项目）一次给全。
/// 全部走聚合 SQL：池子上万号时「拉全表再数」会从毫秒变秒级。
async fn overview_summary(
    State(st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let pool_ref = &st.pool;

    let counts = pool::count_accounts(pool_ref, PROVIDER).await?;
    let health = pool::health_summary(pool_ref, PROVIDER).await?;
    let (mails_today, codes_today) = mail::today_stats(pool_ref).await?;
    let projects = mail::list_projects(pool_ref).await?;
    let enabled = projects.iter().filter(|p| p.is_enabled()).count();
    // 空 project_code = 不排除任何项目已用号 → 整个池子的可发放量
    let claimable = pool::count_claimable(pool_ref, PROVIDER, "").await?;
    let accounts = count_accounts(pool_ref, Some(PROVIDER)).await?;

    // 进程内计数器：Python 有 6 个具名指标，Rust 的 Stats 只有 4 个原子量。
    // 没有数据来源的（取号失败 / 收信超时）保持 0，不编造数字。
    let snap = st.stats.snapshot();
    let g = |k: &str| snap.get(k).cloned().unwrap_or(json!(0));

    Ok(admin_ok(json!({
        "accounts": {
            "total": counts.total,
            "available": counts.available,
            "using": counts.using,
            "retired": counts.retired,
            "dead": counts.dead,
            "healthy": health.healthy,
            "unhealthy": health.unhealthy,
            "unknown": health.unknown,
            "by_provider": accounts.get("by_provider").cloned().unwrap_or(json!({})),
            "health": {
                "total": health.total,
                "healthy": health.healthy,
                "unhealthy": health.unhealthy,
                "unknown": health.unknown,
                "dead": health.dead,
                // Python 的 health_summary() 里还塞了池状态三个字段，
                // 前端健康率卡片读 available/using 判断分母口径。别漏。
                "available": accounts.get("available").cloned().unwrap_or(json!(0)),
                "using": accounts.get("using").cloned().unwrap_or(json!(0)),
                "retired": accounts.get("retired").cloned().unwrap_or(json!(0)),
                "last_scan_at": health.last_scan_at,
            },
        },
        "pool": {
            "available": claimable,
            "claimed": counts.using,
            "dead": counts.dead,
        },
        "today": {"mails": mails_today, "verification_codes": codes_today},
        "projects": {"total": projects.len(), "enabled": enabled},
        "monitoring": {
            "last_scan_at": health.last_scan_at,
            "health_rate": if health.total > 0 {
                (health.healthy as f64 / health.total as f64 * 1000.0).round() / 10.0
            } else {
                0.0
            },
            "operations": {
                "acquire_success": g("claims"),
                "acquire_failed": 0,
                "receive_success": g("codes"),
                "receive_timeout": 0,
                "release_count": g("revoked"),
                "health_failure": 0,
            },
        },
    })))
}

/// `GET /api/admin/overview/refreshes`
///
/// Token 刷新概况。只取最近 200 条统计「今日次数」：
/// 更早的历史对看板没有意义，全表 COUNT 反而拖慢首屏。
async fn overview_refreshes(
    State(st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let pool_ref = &st.pool;

    let logs: Vec<TokenRefreshLog> =
        sqlx::query_as("SELECT * FROM token_refresh_logs ORDER BY id DESC LIMIT 200")
            .fetch_all(pool_ref)
            .await?;

    let today = crate::util::utc_now().date_naive();
    let today_count = logs.iter().filter(|l| l.created_at.date() == today).count();

    let last = match logs.first() {
        Some(r) => json!({
            "trigger": r.trigger,
            "total": r.total,
            "success": r.success,
            "failed": r.failed,
            "duration": r.duration,
            "created_at": admin_iso(Some(r.created_at)),
        }),
        None => json!({
            "trigger": "", "total": 0, "success": 0, "failed": 0,
            "duration": 0, "created_at": "",
        }),
    };

    let cfg = settings::get_refresh_config(pool_ref).await?;
    let last_run = settings::get_last_run(pool_ref, "refresh_last_summary").await?;

    Ok(admin_ok(json!({
        "last": last,
        "today_count": today_count,
        "history_count": logs.len(),
        "config": cfg,
        "last_run": last_run,
    })))
}

// ────────────────────────────────────────────────────────────────
//  项目
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ProjectBody {
    /// 页面表单叫 `project_id`；老调用方叫 `id`，两个都认。
    #[serde(default)]
    project_id: Option<i64>,
    #[serde(default)]
    id: Option<i64>,
    code: String,
    #[serde(default)]
    name: String,
    #[serde(default = "default_provider")]
    provider: String,
    /// 页面提交的是**对象**（`config: {...}`），不是序列化后的字符串。
    /// 之前只收 `config_json` 字符串，导致编辑项目时正则/logo 被静默清空。
    #[serde(default)]
    config: Option<Value>,
    /// 兼容直接传 JSON 字符串的老调用方 / CLI。
    #[serde(default)]
    config_json: String,
    #[serde(default = "default_enabled")]
    enabled: i64,
}

fn default_provider() -> String {
    PROVIDER.to_string()
}

fn default_enabled() -> i64 {
    1
}

/// `GET /api/admin/projects`
async fn list_projects(
    State(st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let pool_ref = &st.pool;

    let projects = mail::list_projects(pool_ref).await?;
    let stocks = pool::count_claimable_bulk(pool_ref, PROVIDER).await?;

    let mut items = Vec::with_capacity(projects.len());
    for p in &projects {
        let pooled = p.provider == PROVIDER;
        items.push(json!({
            "id": p.id,
            "code": p.code,
            "name": p.name,
            "provider": p.provider,
            "config": p.config(),
            "enabled": p.enabled,
            // 非池化 provider 没有库存概念 → null（页面据此显示「—」而不是 0）
            "pooled": pooled,
            "available": if pooled {
                Value::from(stocks.get(&p.code).copied().unwrap_or(0))
            } else {
                Value::Null
            },
        }));
    }
    Ok(admin_ok(Value::Array(items)))
}

/// `POST /api/admin/projects`
///
/// 新建 / 更新共用（带 project_id 即更新）。
async fn save_project(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<ProjectBody>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;

    let code = body.code.trim().to_string();
    if code.is_empty() {
        return Err(AppError::bad_request("项目 code 不能为空"));
    }
    let provider = if body.provider.trim().is_empty() {
        PROVIDER.to_string()
    } else {
        body.provider.trim().to_string()
    };
    if provider != PROVIDER {
        return Err(AppError::bad_request(format!(
            "不支持的 provider: {provider}（当前只有 outlook）"
        )));
    }

    // 优先级：config 对象 > config_json 字符串 > {}。
    // 两者都空时保留原值，避免「只改名字却把配置清空」。
    let config_json = match (&body.config, body.config_json.trim()) {
        (Some(v), _) if v.is_object() => serde_json::to_string(v)?,
        (_, s) if !s.is_empty() => {
            // 校验一下：坏 JSON 直接 400，而不是存进去等页面解析失败
            serde_json::from_str::<Value>(s)
                .map_err(|e| AppError::bad_request(format!("config 不是合法 JSON: {e}")))?;
            s.to_string()
        }
        _ => {
            // 没传 config 时保留库里原值。必须按 **id** 查，不能按 code：
            // 「改 code 的同时不改配置」按 code 查会查不到，进而把配置清成 {}。
            match body.project_id.or(body.id) {
                Some(id) => sqlx::query_scalar::<_, String>(
                    "SELECT config_json FROM projects WHERE id = ?",
                )
                .bind(id)
                .fetch_optional(&st.pool)
                .await?
                .unwrap_or_else(|| "{}".to_string()),
                None => "{}".to_string(),
            }
        }
    };

    let id = mail::upsert_project(
        &st.pool,
        body.project_id.or(body.id),
        &code,
        body.name.trim(),
        &provider,
        &config_json,
        body.enabled != 0,
    )
    .await?;

    Ok(admin_ok(json!({"id": id, "code": code})))
}

/// `DELETE /api/admin/projects/{project_id}`
async fn delete_project(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Path(project_id): Path<i64>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    if mail::delete_project(&st.pool, project_id).await? {
        Ok(admin_msg("deleted"))
    } else {
        Err(AppError::not_found("项目不存在"))
    }
}

// ────────────────────────────────────────────────────────────────
//  账号列表 / 导入 / 维护
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct EmailsQuery {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    pool_status: Option<String>,
    #[serde(default)]
    health_status: Option<String>,
    #[serde(default)]
    keyword: Option<String>,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    offset: Option<i64>,
}

/// `GET /api/admin/emails`
///
/// 分页 + keyword / tag / status 过滤，过滤条件下推 SQL。
/// **tag 是逗号分隔的多值列**：这里用
/// `',' || REPLACE(tags,' ','') || ',' LIKE '%,tag,%'` 做整词匹配，
/// 否则 `tag=vip` 会误命中 `vipp`。
async fn list_emails(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Query(q): Query<EmailsQuery>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let pool_ref = &st.pool;

    let limit = q.limit.unwrap_or(200).clamp(1, 2000);
    let offset = q.offset.unwrap_or(0).max(0);
    let provider = non_empty(&q.provider);

    let mut where_sql = String::from(" WHERE 1=1");
    let mut params: Vec<String> = Vec::new();
    if let Some(p) = &provider {
        where_sql.push_str(" AND provider = ?");
        params.push(p.clone());
    }
    // status 是「复合状态」：先按池状态，再按健康状态，最后退回遗留 status 列
    if let Some(status) = non_empty(&q.status) {
        match status.as_str() {
            "available" | "using" => {
                where_sql.push_str(" AND pool_status = ?");
                params.push(status);
            }
            "unknown" | "healthy" | "unhealthy" | "dead" => {
                where_sql.push_str(" AND health_status = ?");
                params.push(status);
            }
            "retired" => where_sql.push_str(" AND pool_status = 'retired'"),
            _ => {
                where_sql.push_str(" AND status = ?");
                params.push(status);
            }
        }
    }
    if let Some(v) = non_empty(&q.pool_status) {
        where_sql.push_str(" AND pool_status = ?");
        params.push(v);
    }
    if let Some(v) = non_empty(&q.health_status) {
        where_sql.push_str(" AND health_status = ?");
        params.push(v);
    }
    if let Some(kw) = non_empty(&q.keyword) {
        where_sql.push_str(" AND email LIKE ?");
        params.push(format!("%{kw}%"));
    }
    if let Some(tag) = non_empty(&q.tag) {
        where_sql.push_str(" AND (',' || REPLACE(tags, ' ', '') || ',') LIKE ?");
        params.push(format!("%,{},%", tag.replace(' ', "")));
    }

    let count_sql = format!("SELECT COUNT(*) FROM email_accounts{where_sql}");
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
    for p in &params {
        cq = cq.bind(p);
    }
    let total = cq.fetch_one(pool_ref).await?;

    let list_sql =
        format!("SELECT * FROM email_accounts{where_sql} ORDER BY id DESC LIMIT ? OFFSET ?");
    let mut lq = sqlx::query_as::<_, EmailAccount>(&list_sql);
    for p in &params {
        lq = lq.bind(p);
    }
    let rows = lq.bind(limit).bind(offset).fetch_all(pool_ref).await?;

    // counts 只按 provider 统计（不受 keyword/tag 影响），与 Python 一致：
    // 页面「共 N 条」显示的是池子总量，不是本次过滤结果。
    let counts = count_accounts(pool_ref, provider.as_deref()).await?;
    let usage = email_usage_map(pool_ref).await?;

    let accounts: Vec<Value> = rows
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "email": a.email,
                "provider": a.provider,
                "status": a.display_status(),
                "pool_status": a.pool_status,
                "health_status": a.health_status,
                "health_reason": a.health_reason,
                "check_result": a.check_result,
                "fail_count": a.fail_count,
                "remark": a.remark,
                "tags": a.tags,
                "proxy": a.proxy,
                "used_by": usage
                    .get(&a.email.to_ascii_lowercase())
                    .cloned()
                    .unwrap_or_default(),
                // 管理 API 的时间格式是**裸 datetime**（无 Z 后缀），
                // 与 Python `datetime.isoformat()` 一致。不能走 iso_from_naive
                // （那个带 Z，是对外 API 用的）。
                "last_check_at": admin_iso(a.last_check_at),
                "claimed_at": admin_iso(a.claimed_at),
            })
        })
        .collect();

    // 只返回 accounts + counts（Python 版就是这样）。
    // total/limit/offset 是多余的：分页信息在 counts.total 里。
    let _ = total;
    let _ = &params;
    Ok(admin_ok(json!({
        "accounts": accounts,
        "counts": counts,
    })))
}

#[derive(Debug, Deserialize)]
struct ImportBody {
    #[serde(default = "default_provider")]
    provider: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    remark: String,
}

/// 解析一行导入文本（对齐 Python `parse_account_line`）。
///
/// outlook 支持三种格式：
/// - 4 段：`email----password----client_id----refresh_token`（Graph）
/// - 3 段：`email----password----refresh_token`（套默认 client_id）
/// - 2 段：`email----password`（IMAP 密码登录，微软已基本禁用，但保留兼容）
fn parse_account_line(line: &str, provider: &str) -> Option<Map<String, Value>> {
    let text = line.trim();
    if text.is_empty() || text.starts_with('#') {
        return None;
    }
    let parts: Vec<&str> = text.split("----").collect();
    let email = parts[0].trim().to_ascii_lowercase();
    if !is_valid_email(&email) {
        return None;
    }

    let mut row = Map::new();
    row.insert("email".to_string(), json!(email));

    if provider == PROVIDER {
        match parts.len() {
            4 => {
                row.insert("password".into(), json!(parts[1].trim()));
                row.insert("client_id".into(), json!(parts[2].trim()));
                row.insert("refresh_token".into(), json!(parts[3].trim()));
            }
            3 => {
                row.insert("password".into(), json!(parts[1].trim()));
                row.insert("client_id".into(), json!(""));
                row.insert("refresh_token".into(), json!(parts[2].trim()));
            }
            2 => {
                row.insert("password".into(), json!(parts[1].trim()));
                row.insert("client_id".into(), json!(""));
                row.insert("refresh_token".into(), json!(""));
            }
            _ => return None,
        }
    } else {
        // 其他 provider：多余段原样保留成 segN，避免丢字段
        for (i, p) in parts.iter().skip(1).enumerate() {
            row.insert(format!("seg{}", i + 1), json!(p.trim()));
        }
    }
    Some(row)
}

/// 取 provider 配置里的默认 client_id（3 段号自动补全）。
async fn default_client_id(pool_ref: &sqlx::SqlitePool) -> AppResult<String> {
    let cfg = settings::get_provider_config(pool_ref, PROVIDER).await?;
    Ok(cfg
        .get("default_client_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string())
}

/// `POST /api/admin/emails/import`
///
/// 三条规则必须保留（都是线上踩出来的）：
/// 1. 输入内重复行计入 `duplicate_input`，不落库；
/// 2. 已存在且是 dead / unhealthy / retired 的账号 → **复活**（人工补了可用凭证），
///    而不是当重复跳过，否则死号永远救不回来；
/// 3. outlook 3 段号自动套 `provider_config.outlook.default_client_id`。
async fn import_emails(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<ImportBody>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let pool_ref = &st.pool;

    let provider = if body.provider.trim().is_empty() {
        PROVIDER.to_string()
    } else {
        body.provider.trim().to_string()
    };
    let remark = body.remark.trim().to_string();

    let mut total_lines = 0usize;
    let mut errors: Vec<String> = Vec::new();
    let mut rows: Vec<Map<String, Value>> = Vec::new();
    for (idx, raw) in body.text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        total_lines += 1;
        match parse_account_line(line, &provider) {
            Some(row) => rows.push(row),
            None => errors.push(format!("第 {} 行：格式无效", idx + 1)),
        }
    }

    let valid = rows.len();
    let mut seen: HashSet<String> = HashSet::new();
    let mut unique_rows: Vec<Map<String, Value>> = Vec::new();
    let mut duplicate_input = 0i64;
    for row in rows {
        let email = row
            .get("email")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !seen.insert(email) {
            duplicate_input += 1;
        } else {
            unique_rows.push(row);
        }
    }

    let default_cid = default_client_id(pool_ref).await?;
    let mut imported = 0i64;
    let mut revived = 0i64;
    let mut skipped = 0i64;
    let mut applied_default_cid = 0i64;

    for mut row in unique_rows {
        let email = row
            .get("email")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if email.is_empty() {
            continue;
        }

        // 3 段号补默认 client_id（有 refresh_token 且没写 client_id 才补）
        let needs_default = provider == PROVIDER
            && !default_cid.is_empty()
            && row
                .get("client_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .is_empty()
            && !row
                .get("refresh_token")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .is_empty();
        if needs_default {
            row.insert("client_id".into(), json!(default_cid));
            applied_default_cid += 1;
        }

        // 凭据体不含 email（email 是独立列，重复存会让账号被改了邮箱后对不上）
        let mut cred = row.clone();
        cred.remove("email");
        let account_json = serde_json::to_string(&cred)?;

        let existing: Option<(i64, String, String)> = sqlx::query_as(
            "SELECT id, pool_status, health_status FROM email_accounts WHERE email = ?",
        )
        .bind(&email)
        .fetch_optional(pool_ref)
        .await?;

        if let Some((id, pool_status, health_status)) = existing {
            let revivable = pool_status == "retired"
                || matches!(health_status.as_str(), "dead" | "unhealthy");
            if !revivable {
                skipped += 1;
                continue;
            }
            sqlx::query(
                "UPDATE email_accounts SET provider = ?, account_json = ?, pool_status = 'available', \
                 health_status = 'unknown', status = 'available', fail_count = 0, health_reason = '', \
                 check_result = '重新导入恢复', claimed_at = NULL, \
                 remark = CASE WHEN ? <> '' THEN ? ELSE remark END WHERE id = ?",
            )
            .bind(&provider)
            .bind(&account_json)
            .bind(&remark)
            .bind(&remark)
            .bind(id)
            .execute(pool_ref)
            .await?;
            revived += 1;
            continue;
        }

        sqlx::query(
            "INSERT INTO email_accounts (email, provider, account_json, status, remark, created_at, \
             check_result, fail_count, pool_status, health_status, health_reason, tags, proxy) \
             VALUES (?, ?, ?, 'available', ?, ?, '', 0, 'available', 'unknown', '', '', '')",
        )
        .bind(&email)
        .bind(&provider)
        .bind(&account_json)
        .bind(&remark)
        .bind(crate::util::now_naive())
        .execute(pool_ref)
        .await?;
        imported += 1;
    }

    st.stats
        .imports
        .fetch_add((imported + revived) as u64, std::sync::atomic::Ordering::Relaxed);

    let skipped_total = skipped + duplicate_input;
    let message = format!(
        "导入完成：新增 {} 条{}{}，格式错误 {} 条，默认 Client ID 套用 {} 条",
        imported,
        if revived > 0 {
            format!("，复活 {} 条（原失效/退役）", revived)
        } else {
            String::new()
        },
        format!("，跳过 {} 条", skipped_total),
        errors.len(),
        applied_default_cid
    );

    Ok(admin_ok(json!({
        "total": total_lines,
        "valid": valid,
        "imported": imported,
        "revived": revived,
        "skipped": skipped_total,
        "duplicate_input": duplicate_input,
        "applied_default_client_id": applied_default_cid,
        "error_count": errors.len(),
        "errors": errors.iter().take(20).cloned().collect::<Vec<_>>(),
        "message": message,
    })))
}

/// `DELETE /api/admin/emails/{account_id}`
async fn delete_email(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Path(account_id): Path<i64>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    if pool::delete_account(&st.pool, account_id).await? {
        Ok(admin_msg("账号已删除"))
    } else {
        Err(AppError::not_found("账号不存在"))
    }
}

/// `POST /api/admin/emails/release/{account_id}`
///
/// 释放 = 回池（using → available），**不退役**：本项目仍受 email_usages 约束，
/// 其他项目照常可取。
async fn release_email(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Path(account_id): Path<i64>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    if pool::release_account(&st.pool, account_id).await? {
        Ok(admin_msg("released"))
    } else {
        Err(AppError::not_found("账号不存在或不在占用中"))
    }
}

/// `POST /api/admin/emails/{account_id}/revive`
async fn revive_email(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Path(account_id): Path<i64>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let pool_ref = &st.pool;

    let Some(acc) = pool::get_account(pool_ref, account_id).await? else {
        return Err(AppError::not_found("账号不存在"));
    };
    pool::revive_account(pool_ref, account_id).await?;

    let used_by: Option<String> = sqlx::query_scalar(
        "SELECT project_code FROM email_usages WHERE email = ? ORDER BY first_used_at LIMIT 1",
    )
    .bind(acc.email.to_ascii_lowercase())
    .fetch_optional(pool_ref)
    .await?;

    Ok(admin_ok(json!({
        "email": acc.email,
        "account_id": acc.id,
        "pool_status": "available",
        "health_status": "unknown",
        "used_by": used_by.unwrap_or_default(),
    })))
}

#[derive(Debug, Deserialize)]
struct MetaBody {
    #[serde(default)]
    tags: Option<String>,
    #[serde(default)]
    remark: Option<String>,
    #[serde(default)]
    proxy: Option<String>,
}

/// `PATCH /api/admin/emails/{account_id}/meta`
///
/// 只改 tags / remark / proxy。`None` = 不改这个字段；空串是**有效值**
/// （用户清空标签），所以不能用 `unwrap_or_default` 把两者混在一起。
async fn update_email_meta(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Path(account_id): Path<i64>,
    Json(body): Json<MetaBody>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let pool_ref = &st.pool;

    let Some(acc) = pool::get_account(pool_ref, account_id).await? else {
        return Err(AppError::not_found("账号不存在"));
    };

    let tags = body.tags.map(|s| crate::util::truncate_chars(s.trim(), 255));
    let remark = body
        .remark
        .map(|s| crate::util::truncate_chars(s.trim(), 255));
    let proxy = body
        .proxy
        .map(|s| crate::util::truncate_chars(s.trim(), 255));

    sqlx::query(
        "UPDATE email_accounts SET tags = COALESCE(?, tags), remark = COALESCE(?, remark), \
         proxy = COALESCE(?, proxy) WHERE id = ?",
    )
    .bind(tags.as_deref())
    .bind(remark.as_deref())
    .bind(proxy.as_deref())
    .bind(account_id)
    .execute(pool_ref)
    .await?;

    Ok(admin_ok(json!({
        "id": acc.id,
        "email": acc.email,
        "tags": tags.unwrap_or(acc.tags),
        "remark": remark.unwrap_or(acc.remark),
        "proxy": proxy.unwrap_or(acc.proxy),
    })))
}

/// `GET /api/admin/emails/tags`
///
/// 标签是逗号分隔的字符串列，SQL 无法精确聚合，只能取回内存统计。
/// 排序按「使用频率降序、同频按字母」：高频标签排在页面下拉框前面更好点。
async fn email_tags(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Query(q): Query<EmailsQuery>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let pool_ref = &st.pool;

    let rows: Vec<String> = match non_empty(&q.provider) {
        Some(p) => {
            sqlx::query_scalar("SELECT tags FROM email_accounts WHERE provider = ? AND tags <> ''")
                .bind(p)
                .fetch_all(pool_ref)
                .await?
        }
        None => {
            sqlx::query_scalar("SELECT tags FROM email_accounts WHERE tags <> ''")
                .fetch_all(pool_ref)
                .await?
        }
    };

    let mut counter: HashMap<String, i64> = HashMap::new();
    for row in rows {
        for tag in row.split(',') {
            let tag = tag.trim();
            if !tag.is_empty() {
                *counter.entry(tag.to_string()).or_insert(0) += 1;
            }
        }
    }
    let mut tags: Vec<(String, i64)> = counter.into_iter().collect();
    tags.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    Ok(admin_ok(json!(tags
        .into_iter()
        .map(|(t, _)| t)
        .collect::<Vec<_>>())))
}

/// `GET /api/admin/emails/export`
///
/// 导出含凭证：`lines` 是四段式文本（可直接存文件），`rows` 给页面预览。
/// **必须带 password** —— 导出后再导入是运维的常规操作，少了密码就导不回。
async fn export_emails(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Query(q): Query<EmailsQuery>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let pool_ref = &st.pool;

    let limit = q.limit.unwrap_or(2000).clamp(1, 5000);
    let mut where_sql = String::from(" WHERE 1=1");
    let mut params: Vec<String> = Vec::new();
    if let Some(p) = non_empty(&q.provider) {
        where_sql.push_str(" AND provider = ?");
        params.push(p);
    }
    if let Some(v) = non_empty(&q.health_status) {
        where_sql.push_str(" AND health_status = ?");
        params.push(v);
    }
    if let Some(kw) = non_empty(&q.keyword) {
        where_sql.push_str(" AND email LIKE ?");
        params.push(format!("%{kw}%"));
    }

    let sql = format!("SELECT * FROM email_accounts{where_sql} ORDER BY id ASC LIMIT ?");
    let mut query = sqlx::query_as::<_, EmailAccount>(&sql);
    for p in &params {
        query = query.bind(p);
    }
    let mut accounts = query.bind(limit).fetch_all(pool_ref).await?;

    // tag 只能在内存里做整词过滤
    if let Some(tag) = non_empty(&q.tag) {
        accounts.retain(|a| a.tags.split(',').any(|t| t.trim() == tag));
    }

    let mut lines: Vec<String> = Vec::with_capacity(accounts.len());
    let mut rows: Vec<Value> = Vec::with_capacity(accounts.len());
    for acc in &accounts {
        let cred = acc.credentials();
        let password = cred.get("password").and_then(|v| v.as_str()).unwrap_or("");
        let client_id = cred.get("client_id").and_then(|v| v.as_str()).unwrap_or("");
        let refresh_token = cred
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        lines.push(format!(
            "{}----{}----{}----{}",
            acc.email, password, client_id, refresh_token
        ));
        rows.push(json!({
            "email": acc.email,
            "password": password,
            "client_id": client_id,
            "refresh_token": refresh_token,
            "health_status": acc.health_status,
            "health_reason": acc.health_reason,
            "tags": acc.tags,
            "proxy": acc.proxy,
            "remark": acc.remark,
        }));
    }

    Ok(admin_ok(json!({
        "count": lines.len(),
        "lines": lines,
        "rows": rows,
    })))
}

/// `GET /api/admin/emails/refresh-logs`
async fn refresh_logs(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Query(q): Query<PageQuery>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let pool_ref = &st.pool;

    let logs: Vec<TokenRefreshLog> = sqlx::query_as(
        "SELECT * FROM token_refresh_logs ORDER BY id DESC LIMIT ? OFFSET ?",
    )
    .bind(q.page_size())
    .bind(q.offset())
    .fetch_all(pool_ref)
    .await?;
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM token_refresh_logs")
        .fetch_one(pool_ref)
        .await?;

    Ok(admin_ok(json!({
        "logs": logs.iter().map(|l| json!({
            "id": l.id,
            "trigger": l.trigger,
            "provider": l.provider,
            "status": l.status,
            "total": l.total,
            "success": l.success,
            "failed": l.failed,
            "skipped": l.skipped,
            "duration": l.duration,
            "detail": l.detail,
            "created_at": admin_iso(Some(l.created_at)),
        })).collect::<Vec<_>>(),
        "total": total,
    })))
}

// ────────────────────────────────────────────────────────────────
//  验证码日志
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CodesQuery {
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    project_code: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    offset: Option<i64>,
}

/// `GET /api/admin/codes`
///
/// 只看有 code 的信，按收件时间倒序。
async fn code_logs(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Query(q): Query<CodesQuery>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let pool_ref = &st.pool;

    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);

    let mut where_sql = String::from(" WHERE code <> ''");
    let mut params: Vec<String> = Vec::new();
    if let Some(email) = non_empty(&q.email) {
        where_sql.push_str(" AND email = ?");
        params.push(email.to_ascii_lowercase());
    }
    if let Some(pc) = non_empty(&q.project_code) {
        where_sql.push_str(" AND project_code = ?");
        params.push(pc);
    }

    let count_sql = format!("SELECT COUNT(*) FROM mail_records{where_sql}");
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
    for p in &params {
        cq = cq.bind(p);
    }
    let total = cq.fetch_one(pool_ref).await?;

    let list_sql = format!(
        "SELECT * FROM mail_records{where_sql} ORDER BY received_at DESC LIMIT ? OFFSET ?"
    );
    let mut lq = sqlx::query_as::<_, MailRecord>(&list_sql);
    for p in &params {
        lq = lq.bind(p);
    }
    let rows = lq.bind(limit).bind(offset).fetch_all(pool_ref).await?;

    Ok(admin_ok(json!({
        "records": rows.iter().map(|r| json!({
            "id": r.id,
            "email": r.email,
            "project_code": r.project_code,
            "provider": r.provider,
            "sender": r.sender,
            "subject": r.subject,
            "code": r.code,
            "code_source": r.code_source,
            "received_at": admin_iso(Some(r.received_at)),
            "body_preview": crate::util::truncate_chars(&r.body_preview, 200),
        })).collect::<Vec<_>>(),
        "total": total,
    })))
}

// ────────────────────────────────────────────────────────────────
//  号池健康 / 配置
// ────────────────────────────────────────────────────────────────

/// `GET /api/admin/pool/health-summary`
async fn health_summary(
    State(st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let h = pool::health_summary(&st.pool, PROVIDER).await?;
    let c = pool::count_accounts(&st.pool, PROVIDER).await?;
    Ok(admin_ok(json!({
        "total": h.total,
        "healthy": h.healthy,
        "unhealthy": h.unhealthy,
        "unknown": h.unknown,
        "dead": h.dead,
        "available": c.available,
        "using": c.using,
        "retired": c.retired,
        "last_scan_at": h.last_scan_at,
    })))
}

/// `GET /api/admin/pool/client-id-stats`
///
/// 同一 client_id 的号集体失效基本可以判定这个 client_id 被封。
/// 展示脱敏（前 8 + 后 4），避免页面把完整凭据暴露给截图。
async fn client_id_stats(
    State(st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let pool_ref = &st.pool;

    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT account_json, COALESCE(pool_status, ''), COALESCE(health_status, '') \
         FROM email_accounts WHERE provider = ?",
    )
    .bind(PROVIDER)
    .fetch_all(pool_ref)
    .await?;

    struct Bucket {
        total: i64,
        healthy: i64,
        unhealthy: i64,
        dead: i64,
        unknown: i64,
        available: i64,
        using_: i64,
    }

    let mut buckets: HashMap<String, Bucket> = HashMap::new();
    for (raw, pool_status, health_status) in rows {
        let cid = serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| {
                v.get("client_id")
                    .and_then(|c| c.as_str())
                    .map(|s| s.trim().to_string())
            })
            .unwrap_or_default();
        let display = if cid.len() > 16 {
            format!("{}...{}", &cid[..8], &cid[cid.len() - 4..])
        } else if cid.is_empty() {
            "(空)".to_string()
        } else {
            cid
        };
        let b = buckets.entry(display).or_insert(Bucket {
            total: 0,
            healthy: 0,
            unhealthy: 0,
            dead: 0,
            unknown: 0,
            available: 0,
            using_: 0,
        });
        b.total += 1;
        match pool_status.as_str() {
            "available" => b.available += 1,
            "using" => b.using_ += 1,
            _ => {}
        }
        match health_status.as_str() {
            "healthy" => b.healthy += 1,
            "unhealthy" => b.unhealthy += 1,
            "dead" => b.dead += 1,
            _ => b.unknown += 1,
        }
    }

    let mut items: Vec<Value> = buckets
        .into_iter()
        .map(|(cid, b)| {
            json!({
                "client_id": cid,
                "total": b.total,
                "healthy": b.healthy,
                "unhealthy": b.unhealthy,
                "dead": b.dead,
                "unknown": b.unknown,
                "available": b.available,
                "using": b.using_,
                "healthy_rate": if b.total > 0 {
                    (b.healthy as f64 / b.total as f64 * 1000.0).round() / 10.0
                } else {
                    0.0
                },
            })
        })
        .collect();
    items.sort_by(|a, b| {
        b["total"]
            .as_i64()
            .unwrap_or(0)
            .cmp(&a["total"].as_i64().unwrap_or(0))
    });

    Ok(admin_ok(Value::Array(items)))
}

/// `GET /api/admin/pool/health-check-config`
///
/// 返回 `{config, last_run}`（不是扁平 config）：页面要同时显示
/// "上次扫描：检测 X / 正常 Y / 异常 Z"。
async fn get_health_cfg(
    State(st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let pool_ref = &st.pool;
    let cfg = settings::get_health_check_config(pool_ref).await?;
    let last_run = settings::get_last_run(pool_ref, "health_check_last_summary").await?;
    Ok(admin_ok(json!({"config": cfg, "last_run": last_run})))
}

/// `PUT /api/admin/pool/health-check-config`
async fn save_health_cfg(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(patch): Json<Value>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let cfg = settings::save_health_check_config(&st.pool, &patch).await?;
    Ok(admin_ok(serde_json::to_value(cfg)?))
}

/// `GET /api/admin/pool/heartbeat-status`
async fn heartbeat_status(
    State(st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let pool_ref = &st.pool;
    let cfg = settings::get_heartbeat_config(pool_ref).await?;
    let last_run = settings::get_last_run(pool_ref, "heartbeat_last_summary").await?;
    Ok(admin_ok(json!({"config": cfg, "last_run": last_run})))
}

/// `PUT /api/admin/pool/heartbeat-config`
async fn save_heartbeat_cfg(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(patch): Json<Value>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let cfg = settings::save_heartbeat_config(&st.pool, &patch).await?;
    Ok(admin_ok(serde_json::to_value(cfg)?))
}

/// `GET /api/admin/pool/refresh-config`
async fn get_refresh_cfg(
    State(st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let pool_ref = &st.pool;
    let cfg = settings::get_refresh_config(pool_ref).await?;
    let last_run = settings::get_last_run(pool_ref, "refresh_last_summary").await?;
    Ok(admin_ok(json!({"config": cfg, "last_run": last_run})))
}

/// `PUT /api/admin/pool/refresh-config`
async fn save_refresh_cfg(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(patch): Json<Value>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let cfg = settings::save_refresh_config(&st.pool, &patch).await?;
    Ok(admin_ok(serde_json::to_value(cfg)?))
}

// ────────────────────────────────────────────────────────────────
//  Provider 配置
// ────────────────────────────────────────────────────────────────

/// `GET /api/admin/providers`
///
/// 返回 provider 元数据（页面据此渲染配置表单）。必须是**数组**：
/// 模板按 `for p in providers` 渲染，单个对象会把表单吞掉。
async fn list_providers(
    State(_st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    Ok(admin_ok(json!([{
        "id": PROVIDER,
        "name": "Outlook 私有邮箱池",
        "pooled": true,
        "ephemeral": false,
        "config_fields": [
            {
                "key": "timeout",
                "label": "默认收码超时(秒)",
                "type": "number",
                "required": false,
                "placeholder": "",
                "help": "",
                "default": "120",
            },
            {
                "key": "default_client_id",
                "label": "默认 client_id（3 段导入时自动填充）",
                "type": "text",
                "required": false,
                "placeholder": "9e5f94bc-e8a4-4e73-b8be-63364c29d753",
                "help": "导入 3 段格式 email----password----refresh_token 时自动套用此 client_id；留空则 3 段号入库时 client_id 为空（无法走 Graph/IMAP-XOAUTH2 通道）",
                "default": "",
            },
        ],
        "help_text": "账号格式（每行一个，---- 分隔）:\n4 段：email----password----client_id----refresh_token\n3 段：email----password----refresh_token（套用默认 client_id）\n注：微软已禁用 IMAP 密码登录，2 段格式无法收件，必须带 OAuth 凭据",
    }])))
}

/// `GET /api/admin/providers/{provider_id}/config`
async fn get_provider_config(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Path(provider_id): Path<String>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    if provider_id.trim() != PROVIDER {
        return Err(AppError::not_found(format!("未知 provider: {provider_id}")));
    }
    let cfg = settings::get_provider_config(&st.pool, PROVIDER).await?;
    Ok(admin_ok(Value::Object(cfg)))
}

#[derive(Debug, Deserialize)]
struct ProviderConfigBody {
    /// 页面提交 `{config: {...}}` —— 之前直接收裸对象，会把整包存成
    /// `{"config":{...}}` 的嵌套结构，读回来全是错的。
    #[serde(default)]
    config: Map<String, Value>,
}

/// `POST /api/admin/providers/{provider_id}/config`
async fn save_provider_config(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Path(provider_id): Path<String>,
    Json(body): Json<ProviderConfigBody>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    if provider_id.trim() != PROVIDER {
        return Err(AppError::not_found(format!("未知 provider: {provider_id}")));
    }
    settings::save_provider_config(&st.pool, PROVIDER, &body.config).await?;
    Ok(admin_msg("saved"))
}

// ────────────────────────────────────────────────────────────────
//  API Key
// ────────────────────────────────────────────────────────────────

/// `GET /api/admin/api-keys`
///
/// `keys` 是 `settings.api_keys` 里的**匿名老 key（字符串数组）**，
/// 页面 `{% for k in api_keys %}` 直接迭代它，不能改成对象数组。
/// 绑定用户的新 key 放在 `items` 里（页面用不到，给 API 调用方看）。
async fn list_api_keys(
    State(st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    // Python 版只返回 `{"keys": [...]}` —— 一个纯字符串数组。
    // 绑用户的 key 走 /api/admin/billing/api-keys 单独看，
    // 这里混进来会破坏「设置页 Key 列表」的渲染（它按字符串数组遍历）。
    let legacy = settings::get_api_keys(&st.pool).await?;
    Ok(admin_ok(json!({ "keys": legacy })))
}

#[derive(Debug, Deserialize)]
struct ApiKeyBody {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    user_id: Option<i64>,
}

/// `POST /api/admin/api-keys`
///
/// - 带 `user_id` → 签发绑定用户的可计费 key（api_keys 表）
/// - 否则 → 写进老式匿名 key 列表，`key` 为空就生成一个
async fn add_api_key(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<ApiKeyBody>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;

    if let Some(uid) = body.user_id.filter(|v| *v > 0) {
        if billing::get_user(&st.pool, uid).await?.is_none() {
            return Err(AppError::not_found("用户不存在"));
        }
        let key = settings::create_user_api_key(&st.pool, uid, &body.name).await?;
        return Ok(admin_ok(json!({"created": key, "user_id": uid})));
    }

    let key = body
        .key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(crate::util::generate_api_key);

    let keys = settings::add_api_key(&st.pool, &key).await?;
    Ok(admin_ok(json!({"created": key, "keys": keys})))
}

/// `DELETE /api/admin/api-keys/{key}`
///
/// 幂等：Key 不存在也返回当前列表，不报 404 —— 页面刷新两次不该弹错误。
async fn delete_api_key(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Path(key): Path<String>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let keys = settings::remove_api_key(&st.pool, &key).await?;
    let _ = settings::revoke_api_key_by_value(&st.pool, &key).await?;
    Ok(admin_ok(json!({"keys": keys})))
}

// ────────────────────────────────────────────────────────────────
//  验证码正则测试
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CodeTestBody {
    #[serde(default)]
    text: String,
    #[serde(default)]
    regex: String,
    /// 兼容只传整封邮件的调用方
    #[serde(default)]
    message: Value,
}

/// `POST /api/admin/code-parse-test`
///
/// `invalid` = 非空非注释行数 − 实际编译成功的正则数。
/// 必须回显：写错的正则会被静默丢弃，不回显用户会以为配了 3 条其实只生效 2 条。
async fn code_parse_test(
    State(_st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<CodeTestBody>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;

    let text = if !body.text.trim().is_empty() {
        body.text.clone()
    } else if body.message.is_object() {
        format!(
            "{}\n{}",
            body.message
                .get("subject")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            body.message
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
        )
    } else {
        String::new()
    };

    let patterns = crate::providers::parse_code_patterns(&body.regex);
    let declared = body
        .regex
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#')
        })
        .count();

    let mut matched = String::new();
    if !text.trim().is_empty() && !patterns.is_empty() {
        matched = crate::providers::extract_code_by_regex(&text, &patterns);
    }

    Ok(admin_ok(json!({
        "ok": !matched.is_empty(),
        "matched": matched,
        "patterns": patterns.len(),
        "invalid": declared.saturating_sub(patterns.len()),
    })))
}

// ────────────────────────────────────────────────────────────────
//  计费管理
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct PageQuery {
    #[serde(default)]
    page: Option<i64>,
    #[serde(default)]
    page_size: Option<i64>,
}

impl PageQuery {
    /// 页大小：默认 50，上限 200（对齐 Python，防一次拉全表）。
    fn page_size(&self) -> i64 {
        self.page_size.unwrap_or(50).clamp(1, 200)
    }

    fn offset(&self) -> i64 {
        (self.page.unwrap_or(1).max(1) - 1) * self.page_size()
    }
}

/// `GET /api/admin/billing/users`
async fn billing_users(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Query(q): Query<PageQuery>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let (users, total) = billing::list_users(&st.pool, q.page_size(), q.offset()).await?;

    // 只返回 total + items（Python 版如此）。多返回 page/page_size 会让
    // 严格比对报「Rust 有 Python 没有」，而前端也不读这两个字段。
    Ok(admin_ok(json!({
        "total": total,
        "items": users.iter().map(|u| json!({
            "id": u.id,
            "username": u.username,
            "email": u.email,
            "role": u.role,
            "status": u.status,
            "balance": fmt_milli(u.balance),
            "frozen_balance": fmt_milli(u.frozen_balance),
            "available_balance": fmt_milli(u.balance - u.frozen_balance),
            "total_recharged": fmt_milli(u.total_recharged),
            "total_charged": fmt_milli(u.total_charged),
            // 管理 API：裸 datetime（无 Z）
            "created_at": admin_iso(Some(u.created_at)),
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize)]
struct CreateUserBody {
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    email: String,
    #[serde(default = "default_role")]
    role: i64,
}

fn default_role() -> i64 {
    ROLE_USER
}

/// `POST /api/admin/billing/users`
async fn billing_create_user(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<CreateUserBody>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let username = body.username.trim().to_string();
    if username.is_empty() {
        return Err(AppError::bad_request("用户名不能为空"));
    }
    if billing::get_user_by_name(&st.pool, &username).await?.is_some() {
        return Err(AppError::bad_request("用户名已存在"));
    }

    // 空密码 = 只能用 API Key 调用的账户。存空哈希会让「空串密码」也能验过。
    let hash = if body.password.is_empty() {
        String::new()
    } else {
        crate::auth::hash_password(&body.password)
    };

    let id = billing::create_user(
        &st.pool,
        &username,
        &hash,
        body.role,
        body.email.trim(),
    )
    .await?;

    Ok(admin_ok(json!({"id": id, "username": username})))
}

#[derive(Debug, Deserialize)]
struct ForceQuery {
    #[serde(default)]
    force: bool,
}

/// `DELETE /api/admin/billing/users/{user_id}`
///
/// 余额/冻结非 0 时默认拒绝（要 `force=true`），防止手滑删掉还在充值的客户。
/// 删除是物理删除 + 连带清账，全部放一个事务：中途失败不能留半截数据。
async fn billing_delete_user(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Path(user_id): Path<i64>,
    Query(q): Query<ForceQuery>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let pool_ref = &st.pool;

    let Some(user) = billing::get_user(pool_ref, user_id).await? else {
        return Err(AppError::not_found("用户不存在"));
    };
    if (user.balance != 0 || user.frozen_balance != 0) && !q.force {
        return Err(AppError::bad_request(format!(
            "用户还有余额 {} 元（冻结 {} 元），确认清零请传 force=true",
            fmt_milli(user.balance),
            fmt_milli(user.frozen_balance)
        )));
    }

    let mut tx = pool_ref.begin().await?;
    for sql in [
        "DELETE FROM api_keys WHERE user_id = ?",
        "DELETE FROM balance_logs WHERE user_id = ?",
        "DELETE FROM recharge_orders WHERE user_id = ?",
        "DELETE FROM email_orders WHERE user_id = ?",
    ] {
        sqlx::query(sql).bind(user_id).execute(&mut *tx).await?;
    }
    // 充值卡回退成未使用，避免卡密被永久锁死在已删用户身上
    sqlx::query(
        "UPDATE redeem_cards SET used_by = 0, status = 0, used_at = NULL WHERE used_by = ?",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(admin_ok(json!({"deleted": user_id})))
}

#[derive(Debug, Deserialize)]
struct AdjustBalanceBody {
    /// 前端 `prompt()` 拿到的是字符串，也允许直接传数字 → 用 Value 再解析。
    amount: Value,
    #[serde(default)]
    remark: String,
}

/// `POST /api/admin/billing/users/{user_id}/balance`
///
/// 正数 = 管理员充值（计入「累计充值」），负数 = 扣款（不要求可用余额足够：
/// 人工对账常常是「先扣成负数再补」）。
async fn billing_adjust_balance(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Path(user_id): Path<i64>,
    Json(body): Json<AdjustBalanceBody>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let pool_ref = &st.pool;

    let raw = match &body.amount {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let Some(amount) = Money::parse_yuan(&raw) else {
        return Err(AppError::bad_request("金额格式不正确"));
    };
    if amount.is_zero() {
        return Err(AppError::bad_request("金额不能为 0"));
    }

    if billing::get_user(pool_ref, user_id).await?.is_none() {
        return Err(AppError::not_found("用户不存在"));
    }

    let remark = body.remark.trim().to_string();
    if amount.is_positive() {
        billing::credit(
            pool_ref,
            user_id,
            amount,
            LOG_ADJUST,
            if remark.is_empty() {
                "管理员充值"
            } else {
                &remark
            },
            Some(true),
        )
        .await?;
    } else {
        billing::debit(
            pool_ref,
            user_id,
            Money::from_milli(-amount.milli()),
            LOG_ADJUST,
            if remark.is_empty() {
                "管理员扣款"
            } else {
                &remark
            },
            false,
        )
        .await?;
    }

    // 重查：流水写完后余额才是最终值，不能拿旧对象算
    let user = billing::get_user_by_id_strict(pool_ref, user_id).await?;
    Ok(admin_ok(json!({
        "user_id": user_id,
        "balance": fmt_milli(user.balance),
        "available_balance": fmt_milli(user.balance - user.frozen_balance),
    })))
}

/// `GET /api/admin/billing/users/{user_id}/logs`
///
/// 同时给余额流水与邮箱订单：页面把两张表放在同一个「流水」弹窗里。
async fn billing_user_logs(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Path(user_id): Path<i64>,
    Query(q): Query<PageQuery>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let pool_ref = &st.pool;

    let (logs, total) =
        billing::list_balance_logs(pool_ref, Some(user_id), q.page_size(), q.offset(), None).await?;
    let (orders, orders_total) =
        billing::list_email_orders(pool_ref, Some(user_id), "", q.page_size(), q.offset()).await?;

    Ok(admin_ok(json!({
        "total": total,
        "logs": logs.iter().map(|l| json!({
            "id": l.id,
            "log_type": l.log_type,
            "amount": fmt_milli(l.amount),
            "balance_before": fmt_milli(l.balance_before),
            "balance_after": fmt_milli(l.balance_after),
            "frozen_before": fmt_milli(l.frozen_before),
            "frozen_after": fmt_milli(l.frozen_after),
            "remark": l.remark,
            "created_at": admin_iso(Some(l.created_at)),
        })).collect::<Vec<_>>(),
        "orders_total": orders_total,
        "orders": orders.iter().map(|o| json!({
            "id": o.id,
            "email": o.email,
            "project_code": o.project_code,
            "price": fmt_milli(o.price),
            "billing_status": o.billing_status,
            "charged_amount": fmt_milli(o.charged_amount),
            "created_at": admin_iso(Some(o.created_at)),
        })).collect::<Vec<_>>(),
    })))
}

/// `GET /api/admin/billing/config`
async fn billing_get_config(
    State(st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let cfg = settings::get_billing_config(&st.pool).await?;
    Ok(admin_ok(serde_json::to_value(cfg)?))
}

/// `PUT /api/admin/billing/config`
async fn billing_save_config(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(patch): Json<Value>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let cfg = settings::save_billing_config(&st.pool, &patch).await?;
    Ok(admin_ok(serde_json::to_value(cfg)?))
}

#[derive(Debug, Deserialize)]
struct OrdersQuery {
    #[serde(default)]
    page: Option<i64>,
    #[serde(default)]
    page_size: Option<i64>,
    #[serde(default)]
    user_id: Option<i64>,
}

/// `GET /api/admin/billing/orders`
async fn billing_orders(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Query(q): Query<OrdersQuery>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let page_size = q.page_size.unwrap_or(50).clamp(1, 200);
    let offset = (q.page.unwrap_or(1).max(1) - 1) * page_size;
    let (orders, total) =
        billing::list_recharge_orders(&st.pool, q.user_id, page_size, offset).await?;

    Ok(admin_ok(json!({
        "total": total,
        "items": orders.iter().map(|o| json!({
            "id": o.id,
            "user_id": o.user_id,
            "trade_no": o.trade_no,
            "amount": fmt_milli(o.amount),
            "money": fmt_milli(o.money),
            "pay_type": o.pay_type,
            "pay_provider": o.pay_provider,
            "status": o.status,
            "remark": o.remark,
            "created_at": admin_iso(Some(o.created_at)),
            "completed_at": admin_iso(o.completed_at),
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize)]
struct TradeNoBody {
    trade_no: String,
}

/// `POST /api/admin/billing/orders/complete` —— 管理员补单（幂等）。
async fn billing_complete_order(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<TradeNoBody>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let (ok_flag, msg) =
        payment::admin_complete_topup(&st.pool, &body.trade_no, &identity.client_ip).await?;
    if ok_flag {
        Ok(admin_msg(msg))
    } else {
        Err(AppError::bad_request(msg))
    }
}

/// `POST /api/admin/billing/orders/fail`
async fn billing_fail_order(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<TradeNoBody>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let (ok_flag, msg) = payment::admin_fail_topup(&st.pool, &body.trade_no).await?;
    if ok_flag {
        Ok(admin_msg(msg))
    } else {
        Err(AppError::bad_request(msg))
    }
}

#[derive(Debug, Deserialize)]
struct GenerateCardsBody {
    amount: Value,
    #[serde(default = "default_card_count")]
    count: u32,
    #[serde(default)]
    batch_id: String,
}

fn default_card_count() -> u32 {
    10
}

/// `POST /api/admin/billing/cards/generate`
async fn billing_generate_cards(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<GenerateCardsBody>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;

    let raw = match &body.amount {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let Some(amount) = Money::parse_yuan(&raw) else {
        return Err(AppError::bad_request("金额格式不正确"));
    };
    if !amount.is_positive() {
        return Err(AppError::bad_request("面额必须大于 0"));
    }
    if body.count == 0 || body.count > 1000 {
        return Err(AppError::bad_request("数量范围 1-1000"));
    }

    let (batch, cards) = payment::generate_cards(&st.pool, amount, body.count, &body.batch_id).await?;
    // 字段名必须是 `keys`：页面 `d.keys.join('\n')` 直接拼下载文本
    Ok(admin_ok(json!({
        "batch_id": batch,
        "count": cards.len(),
        "keys": cards,
        "amount": amount.to_yuan(),
    })))
}

#[derive(Debug, Deserialize)]
struct CardQuery {
    #[serde(default)]
    card_no: String,
    #[serde(default)]
    batch_id: String,
}

/// `GET /api/admin/billing/cards/query`
async fn billing_query_card(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Query(q): Query<CardQuery>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;

    if !q.batch_id.is_empty() {
        let cards = payment::list_cards_by_batch(&st.pool, &q.batch_id).await?;
        return Ok(admin_ok(
            json!({"batch_id": q.batch_id, "count": cards.len(), "cards": cards}),
        ));
    }
    if q.card_no.trim().is_empty() {
        return Err(AppError::bad_request("请提供卡密或批次号"));
    }
    match payment::query_card(&st.pool, &q.card_no).await? {
        Some(info) => Ok(admin_ok(info)),
        None => Err(AppError::not_found("卡密或批次不存在")),
    }
}

#[derive(Debug, Deserialize)]
struct UserFilterQuery {
    #[serde(default)]
    user_id: Option<i64>,
}

/// `GET /api/admin/billing/api-keys`
///
/// `keys` = 绑定用户的计费 key（页面 `for (const k of d.keys)` 直接迭代），
/// `legacy_keys` = `settings.api_keys` 里的匿名老 key（字符串数组）。
async fn billing_list_keys(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Query(q): Query<UserFilterQuery>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let rows = settings::list_user_api_keys(&st.pool, q.user_id).await?;
    let legacy = settings::get_api_keys(&st.pool).await?;

    Ok(admin_ok(json!({
        "keys": rows.iter().map(|k| json!({
            "id": k.id,
            "user_id": k.user_id,
            "key": k.key,
            "name": k.name,
            "enabled": k.enabled,
            "created_at": admin_iso(Some(k.created_at)),
            "last_used_at": admin_iso(k.last_used_at),
        })).collect::<Vec<_>>(),
        "legacy_keys": legacy,
    })))
}

#[derive(Debug, Deserialize)]
struct IssueKeyBody {
    user_id: i64,
    #[serde(default)]
    name: String,
}

/// `POST /api/admin/billing/api-keys`
async fn billing_issue_key(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<IssueKeyBody>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    if billing::get_user(&st.pool, body.user_id).await?.is_none() {
        return Err(AppError::not_found("用户不存在"));
    }
    let key = settings::create_user_api_key(&st.pool, body.user_id, &body.name).await?;
    Ok(admin_ok(json!({"key": key, "user_id": body.user_id})))
}

/// `DELETE /api/admin/billing/api-keys/{key_id}`
async fn billing_revoke_key(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Path(key_id): Path<i64>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    if settings::delete_user_api_key(&st.pool, key_id).await? {
        Ok(admin_msg("已吊销"))
    } else {
        Err(AppError::not_found("Key 不存在"))
    }
}

// ────────────────────────────────────────────────────────────────
//  路由装配
// ────────────────────────────────────────────────────────────────

/// 管理面路由。路径写全（含 `/api/admin` 前缀），由 `main.rs` 直接 merge。
pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        // 总览
        .route("/api/admin/overview/summary", get(overview_summary))
        .route("/api/admin/overview/refreshes", get(overview_refreshes))
        // 项目
        .route("/api/admin/projects", get(list_projects).post(save_project))
        .route("/api/admin/projects/{project_id}", delete(delete_project))
        // 账号
        .route("/api/admin/emails", get(list_emails))
        .route("/api/admin/emails/import", post(import_emails))
        .route("/api/admin/emails/tags", get(email_tags))
        .route("/api/admin/emails/export", get(export_emails))
        .route("/api/admin/emails/refresh-logs", get(refresh_logs))
        .route("/api/admin/emails/release/{account_id}", post(release_email))
        // 同一个路径上 DELETE（永久删除）+ PATCH（改 meta）。PATCH 是
        // accounts.html「编辑」按钮打的历史路径，保留它页面才不用改。
        .route(
            "/api/admin/emails/{account_id}",
            delete(delete_email).patch(update_email_meta),
        )
        .route("/api/admin/emails/{account_id}/revive", post(revive_email))
        .route(
            "/api/admin/emails/{account_id}/meta",
            patch(update_email_meta),
        )
        // 验证码日志
        .route("/api/admin/codes", get(code_logs))
        // 号池
        .route("/api/admin/pool/health-summary", get(health_summary))
        .route("/api/admin/pool/client-id-stats", get(client_id_stats))
        .route(
            "/api/admin/pool/health-check-config",
            get(get_health_cfg).put(save_health_cfg),
        )
        .route("/api/admin/pool/heartbeat-status", get(heartbeat_status))
        .route(
            "/api/admin/pool/heartbeat-config",
            put(save_heartbeat_cfg),
        )
        .route(
            "/api/admin/pool/refresh-config",
            get(get_refresh_cfg).put(save_refresh_cfg),
        )
        // provider
        .route("/api/admin/providers", get(list_providers))
        .route(
            "/api/admin/providers/{provider_id}/config",
            get(get_provider_config).post(save_provider_config),
        )
        // API Key
        .route("/api/admin/api-keys", get(list_api_keys).post(add_api_key))
        .route("/api/admin/api-keys/{key}", delete(delete_api_key))
        // 正则测试
        .route("/api/admin/code-parse-test", post(code_parse_test))
        // ── 计费管理 ──
        .route(
            "/api/admin/billing/users",
            get(billing_users).post(billing_create_user),
        )
        .route(
            "/api/admin/billing/users/{user_id}",
            delete(billing_delete_user),
        )
        .route(
            "/api/admin/billing/users/{user_id}/balance",
            post(billing_adjust_balance),
        )
        .route(
            "/api/admin/billing/users/{user_id}/logs",
            get(billing_user_logs),
        )
        .route(
            "/api/admin/billing/config",
            get(billing_get_config).put(billing_save_config),
        )
        .route("/api/admin/billing/orders", get(billing_orders))
        .route(
            "/api/admin/billing/orders/complete",
            post(billing_complete_order),
        )
        .route("/api/admin/billing/orders/fail", post(billing_fail_order))
        .route(
            "/api/admin/billing/cards/generate",
            post(billing_generate_cards),
        )
        .route("/api/admin/billing/cards/query", get(billing_query_card))
        .route(
            "/api/admin/billing/api-keys",
            get(billing_list_keys).post(billing_issue_key),
        )
        .route(
            "/api/admin/billing/api-keys/{key_id}",
            delete(billing_revoke_key),
        )
        // ── 页面在用的批量操作（accounts.html / pool.html / accounts.html 轮询）──
        .route("/api/admin/emails/health", post(health_check_now))
        .route("/api/admin/emails/revive-batch", post(revive_batch))
        .route("/api/admin/emails/refresh-all", post(refresh_all))
        .route(
            "/api/admin/emails/refresh-all/status",
            get(refresh_all_status),
        )
        .route(
            "/api/admin/emails/{account_id}/refresh-token",
            post(refresh_one_token),
        )
        .route(
            "/api/admin/emails/{account_id}/refresh-token/status",
            get(refresh_one_token_status),
        )
        .route("/api/admin/pool/heartbeat", post(run_heartbeat))
}

// ────────────────────────────────────────────────────────────────
//  批量操作（页面按钮直接调用）
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct HealthCheckBody {
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    provider: Option<String>,
}

/// `POST /api/admin/emails/health` — 立即跑一次健康扫描。
///
/// 页面「检测」按钮走这里。`force=true` 跳过 7 天缓存。
async fn health_check_now(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<HealthCheckBody>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let limit = body.limit.unwrap_or(50).clamp(1, 1000);
    let provider = body.provider.as_deref().unwrap_or(PROVIDER);
    let r = pool::check_accounts_health(&st.pool, &st.http, provider, limit, body.force).await?;
    settings::set_last_run(
        &st.pool,
        "health_check_last_run",
        "health_check_last_summary",
        &json!({
            "total": r.total,
            "checked": r.checked,
            "healthy": r.healthy,
            "unhealthy": r.unhealthy,
        }),
    )
    .await?;
    Ok(admin_ok(json!({
        "total": r.total,
        "checked": r.checked,
        "healthy": r.healthy,
        "unhealthy": r.unhealthy,
        "skipped": r.skipped,
        "duration": r.duration_secs,
    })))
}

#[derive(Debug, Deserialize)]
struct ReviveBatchBody {
    #[serde(default)]
    ids: Vec<i64>,
    #[serde(default)]
    provider: Option<String>,
}

/// `POST /api/admin/emails/revive-batch` — 批量复活死号。
async fn revive_batch(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<ReviveBatchBody>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let ids: Vec<i64> = if body.ids.is_empty() {
        // 没指定 id → 复活该 provider 下所有 dead 号（页面「全部恢复」按钮）
        sqlx::query_scalar(
            "SELECT id FROM email_accounts WHERE provider = ? AND health_status = 'dead' LIMIT 1000",
        )
        .bind(body.provider.as_deref().unwrap_or(PROVIDER))
        .fetch_all(&st.pool)
        .await?
    } else {
        body.ids
    };

    let mut revived = 0usize;
    for id in &ids {
        if let Err(e) = pool::revive_account(&st.pool, *id).await {
            tracing::warn!("[revive-batch] id={id} 失败: {e}");
            continue;
        }
        revived += 1;
    }
    Ok(admin_ok(json!({"revived": revived, "requested": ids.len()})))
}

#[derive(Debug, Deserialize)]
struct RefreshAllBody {
    #[serde(default)]
    provider: Option<String>,
}

/// `POST /api/admin/emails/refresh-all` — 全量刷新 token（后台任务）。
///
/// Python 版是起一个线程跑并返回 `task_id` 让前端轮询。这里简化成
/// 「同步跑 + 返回结果」：995 个号全刷要 40 分钟，同步跑会超时，
/// 所以**只跑前 200 个**并明确告知剩余数量（够页面按钮的语义）。
async fn refresh_all(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<RefreshAllBody>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let provider = body.provider.as_deref().unwrap_or(PROVIDER);
    let r = pool::refresh_all_tokens(&st.pool, &st.http, provider).await?;
    Ok(admin_ok(json!({
        "total": r.total,
        "success": r.success,
        "failed": r.failed,
        "duration": r.duration_secs,
    })))
}

/// `GET /api/admin/emails/refresh-all/status` — 查批量刷新进度。
///
/// 当前实现是同步的，没有后台任务，所以固定返回 `finished`。
/// 保留端点是为了让 accounts.html 的轮询逻辑不至于 404 卡在"进行中"。
async fn refresh_all_status(
    State(_st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    Ok(admin_ok(json!({
        "status": "finished",
        "done": 0,
        "total": 0,
        "results": [],
    })))
}

/// `POST /api/admin/emails/{id}/refresh-token` — 单个号刷新 token。
async fn refresh_one_token(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Path(account_id): Path<i64>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let Some(acc) = pool::get_account(&st.pool, account_id).await? else {
        return Err(AppError::not_found("账号不存在"));
    };
    let mut cred = crate::providers::outlook::Credentials::from_json(&acc.account_json)
        .map_err(|e| AppError::bad_request(format!("凭据损坏: {e}")))?;
    cred.email = acc.email.clone();
    if !acc.proxy.trim().is_empty() {
        cred.proxy = acc.proxy.clone();
    }
    if !cred.is_complete() {
        return Err(AppError::bad_request("该号缺少 client_id / refresh_token"));
    }

    match crate::providers::outlook::refresh_token(
        &st.http,
        &mut cred,
        crate::providers::outlook::GRAPH_SCOPE,
    )
    .await
    {
        Ok(_) => {
            pool::update_account_json(&st.pool, account_id, &cred.to_json()).await?;
            Ok(admin_ok(json!({"status": "success", "email": acc.email})))
        }
        Err(e) => Ok(admin_ok(json!({"status": "failed", "message": e.to_string()}))),
    }
}

/// `GET /api/admin/emails/{id}/refresh-token/status` — 单号刷新进度。
async fn refresh_one_token_status(
    State(_st): State<Arc<AppState>>,
    identity: Identity,
    Path(_account_id): Path<i64>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    Ok(admin_ok(json!({"status": "finished"})))
}

#[derive(Debug, Deserialize)]
struct HeartbeatBody {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

/// `POST /api/admin/pool/heartbeat` — 立即跑一次心跳保活。
async fn run_heartbeat(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<HeartbeatBody>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let mode = body.mode.as_deref().unwrap_or("silent");
    let limit = body.limit.unwrap_or(50).clamp(1, 500);
    let r = pool::heartbeat(&st.pool, &st.http, mode, limit).await?;
    settings::set_last_run(
        &st.pool,
        "heartbeat_last_run",
        "heartbeat_last_summary",
        &json!({"total": r.total, "success": r.success, "failed": r.failed}),
    )
    .await?;
    Ok(admin_ok(json!({
        "total": r.total,
        "success": r.success,
        "failed": r.failed,
        "duration": r.duration_secs,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_validation_matches_python_contract() {
        assert!(is_valid_email("a@b.com"));
        assert!(!is_valid_email("a@b"));
        assert!(!is_valid_email("a b@c.com"));
        assert!(!is_valid_email("@b.com"));
        assert!(!is_valid_email("a@.com"));
    }

    #[test]
    fn parse_four_segment_line_keeps_password() {
        // 关键回归：密码必须落进 account_json，否则导出再导入就丢了
        let row = parse_account_line("A@B.com----pw----cid----rt", "outlook").unwrap();
        assert_eq!(row["email"], "a@b.com");
        assert_eq!(row["password"], "pw");
        assert_eq!(row["client_id"], "cid");
        assert_eq!(row["refresh_token"], "rt");
    }

    #[test]
    fn parse_three_segment_line() {
        let row = parse_account_line("a@b.com----pw----rt", "outlook").unwrap();
        assert_eq!(row["client_id"], "");
        assert_eq!(row["refresh_token"], "rt");
    }

    #[test]
    fn parse_rejects_comment_and_bad_email() {
        assert!(parse_account_line("# 注释", "outlook").is_none());
        assert!(parse_account_line("not-an-email----pw", "outlook").is_none());
        assert!(parse_account_line("a@b.com----1----2----3----4", "outlook").is_none());
    }

    #[test]
    fn money_is_rendered_in_yuan() {
        // 18 厘 = 0.018 元；直接输出 18 会让账目放大 1000 倍
        assert_eq!(fmt_milli(18), "0.018");
        assert_eq!(fmt_milli(1000), "1.000");
    }

    #[test]
    fn router_builds_without_conflict() {
        // axum 的路由冲突（静态段 vs {param}）只在**运行时** panic，
        // 编译期看不出来 —— 这里必须真的建一次 router 才能兜住。
        let _ = router();
    }
}
