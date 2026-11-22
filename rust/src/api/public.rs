//! 对外 API：`/api/v1/*` + `/api/product/*`（对齐 MailNest 客户端契约）。
//!
//! 响应格式固定：
//! ```json
//! {"code":"00000","msg":"","type":"","data":...}
//! ```
//! 业务错误也返回 HTTP 200，错误码在 `code` 里（`D0001` 余额不足 …）。
//! **只有**鉴权失败/服务器错误才用 HTTP 4xx/5xx。
//!
//! ## 收码接口的关键契约
//!
//! `receive` 等到没信时要返回 `code=00000` + `status=timeout`，
//! **不能返回错误**。否则客户端会在「对方还没发信」时中断整个注册流程。
//! 这条线上踩过，测试里锁死。

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::AppState;
use crate::error::*;
use crate::services::{billing, mail, pool, settings};

// ────────────────────────────────────────────────────────────────
//  查询可用项目（无需鉴权）
// ────────────────────────────────────────────────────────────────

/// `GET /api/product/info`
///
/// 免鉴权公开接口（对齐 MailNest）。**性能关键**：原 Python 版
/// 每个项目跑一次全表扫描 → 173 条 SQL；这里用批量查询压到 3 条。
pub async fn product_info(State(st): State<std::sync::Arc<AppState>>) -> AppResult<Json<Value>> {
    let pool_ref = &st.pool;

    let billing_cfg = settings::get_billing_config(pool_ref).await?;
    let billing_on = billing_cfg.is_enabled();

    let projects = mail::list_enabled_projects(pool_ref).await?;

    // 一次算出所有项目的库存（3 条 SQL，而不是 55 次全表扫）
    let stocks = pool::count_claimable_bulk(pool_ref, "outlook").await?;

    let mut temporary = Vec::with_capacity(projects.len());
    for p in projects {
        let cfg = p.config();
        let stock = stocks.get(&p.code).copied();
        let logo = cfg
            .get("logo")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // 单价：项目 config 优先，其次全局默认（复用同一次查询的 billing_cfg）
        let price = billing::project_price(pool_ref, &p, Some(&billing_cfg)).await?;
        let original = cfg
            .get("original_price")
            .and_then(|v| v.as_str())
            .map(|s| crate::money::Money::parse_yuan(s).unwrap_or_default())
            .unwrap_or_default();

        // 空 logo 必须是 **null**（不是空串）：Python 版是
        // `project_logo_src(...) or None`，前端据此判断要不要显示首字母占位。
        // 返回空串会让前端渲染一个 src="" 的坏图片。
        let icon_src = crate::config::project_logo_src(logo);
        let icon = if icon_src.is_empty() {
            Value::Null
        } else {
            Value::String(icon_src)
        };

        temporary.push(json!({
            "code": p.code,
            "name": p.name,
            "query": cfg.get("query").and_then(|v| v.as_str()).unwrap_or(""),
            "icon": icon,
            "stock": stock,
            "price": if billing_on { price.to_yuan() } else { String::new() },
            "original_price": if billing_on { original.to_yuan() } else { String::new() },
            "sender_domains": cfg.get("sender_domains").and_then(|v| v.as_str()).unwrap_or(""),
            "mail_keywords": cfg.get("mail_keywords").and_then(|v| v.as_str()).unwrap_or(""),
            "duration_seconds": Value::Null,
        }));
    }

    Ok(ok(json!({
        "temporary": temporary,
        "exclusive": {"stock": null, "price": "", "original_price": ""},
    })))
}

// ────────────────────────────────────────────────────────────────
//  取号
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct BuyBody {
    pub project_code: String,
    #[serde(default = "default_one")]
    pub count: u32,
}

fn default_one() -> u32 {
    1
}

/// `POST /api/v1/email/temporary/buy`
///
/// 计费：计费开启且 key 绑了用户时，取号前先冻结（hold）。
/// 余额不足 → 直接 D0001，**不会**把号发出去。
pub async fn buy_email(
    State(st): State<std::sync::Arc<AppState>>,
    identity: crate::auth::Identity,
    Json(body): Json<BuyBody>,
) -> AppResult<Json<Value>> {
    if body.count < 1 || body.count > 100 {
        return Ok(api_err(CODE_OP_FAILED, "count 取值范围 1-100"));
    }

    let pool_ref = &st.pool;
    // 项目不存在要返回 **D0003 业务码 + HTTP 200**，不是 400。
    // 客户端只认 `code` 字段，HTTP 400 会让它当成网络层错误直接抛出，
    // 而不是走「项目不存在」的分支逻辑。
    let Some(project) = mail::get_enabled_project(pool_ref, body.project_code.trim()).await?
    else {
        return Ok(api_err(CODE_NO_PROJECT, "项目不存在或已停用"));
    };

    let billing_cfg = settings::get_billing_config(pool_ref).await?;
    let billing_on = billing_cfg.is_enabled();
    let price = if billing_on {
        billing::project_price(pool_ref, &project, Some(&billing_cfg)).await?
    } else {
        crate::money::Money::ZERO
    };
    let user_id = if billing_on { identity.user_id } else { None };

    // 余额预检（批量按单价 × count 估）
    if billing_on {
        if let Some(uid) = user_id {
            if price.is_positive() {
                if let Some(u) = billing::get_user(pool_ref, uid).await? {
                    let need = price.milli() * body.count as i64;
                    if u.available_balance().milli() < need {
                        return Ok(api_err(
                            CODE_BALANCE,
                            format!(
                                "余额不足，需要 {} 元，可用 {} 元",
                                crate::money::Money::from_milli(need).to_yuan(),
                                u.available_balance().to_yuan()
                            ),
                        ));
                    }
                }
            }
        }
    }

    // 逐个取号（单个失败不影响已成功的）
    let mut items: Vec<Value> = Vec::new();
    let mut failure: Option<(String, String)> = None;

    for _ in 0..body.count {
        let http = match crate::providers::outlook::build_http_client("") {
            Ok(c) => c,
            Err(e) => {
                failure = Some((CODE_SYSTEM.to_string(), e.to_string()));
                break;
            }
        };
        let _ = http;

        match acquire_one(pool_ref, &project.code).await {
            Ok(item) => items.push(item),
            Err(AppError::BadRequest(msg)) if msg.contains("无可用账号") => {
                failure = Some((CODE_NO_STOCK.to_string(), "邮箱库存不足".to_string()));
                break;
            }
            Err(AppError::BadRequest(msg)) => {
                failure = Some((CODE_NO_STOCK.to_string(), msg));
                break;
            }
            Err(e) => {
                failure = Some((CODE_SYSTEM.to_string(), e.to_string()));
                break;
            }
        }
    }

    // 取号成功后再冻结（每号一条订单）
    if billing_on {
        if let Some(uid) = user_id {
            let mut charged = Vec::new();
            for item in items {
                let email = item
                    .get("email")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                match billing::hold(pool_ref, uid, &email, &project.code, &project.provider, price)
                    .await
                {
                    Ok(_) => charged.push(item),
                    Err(AppError::InsufficientBalance { need, available }) => {
                        failure = Some((
                            CODE_BALANCE.to_string(),
                            format!("余额不足，需要 {need} 元，可用 {available} 元"),
                        ));
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("冻结失败 email={email}: {e}");
                    }
                }
            }
            items = charged;
        }
    }

    st.stats
        .claims
        .fetch_add(items.len() as u64, std::sync::atomic::Ordering::Relaxed);

    if let Some((code, msg)) = failure {
        if items.is_empty() {
            return Ok(api_err(&code, msg));
        }
        return Ok(ok(json!({
            "status": "partial",
            "requested": body.count,
            "succeeded": items.len(),
            "failed": body.count as usize - items.len(),
            "items": items,
            "failure": {"code": code, "message": msg},
        })));
    }

    if body.count > 1 {
        Ok(ok(Value::Array(items)))
    } else {
        Ok(ok(items.into_iter().next().unwrap_or(Value::Null)))
    }
}

/// 取一个号（含基线快照）。
async fn acquire_one(pool_ref: &sqlx::SqlitePool, project_code: &str) -> AppResult<Value> {
    // 1) 取号（内部先原子占位再预检）
    let claimed = pool::claim_account(pool_ref, "outlook", project_code, |_acc| async {
        // 预检延迟到绑定后再做（这里先放行，见下方 verify）
        (true, String::new())
    })
    .await?
    .ok_or_else(|| AppError::bad_request("邮箱池无可用账号（本项目用过的邮箱已排除）"))?;

    // 2) 绑定（按项目一次性）
    let (ok_bind, other) = pool::bind_email(pool_ref, &claimed.email, project_code).await?;
    if !ok_bind {
        pool::release_account(pool_ref, claimed.account_id).await?;
        return Err(AppError::bad_request(format!(
            "邮箱 {} 已被项目 {} 使用过，已跳过该号",
            claimed.email,
            other.unwrap_or_default()
        )));
    }

    // 3) 抓收件箱基线（防旧码）——失败不阻断取号
    if let Some(acc) = pool::get_account(pool_ref, claimed.account_id).await? {
        if let Ok(mut cred) = crate::providers::outlook::Credentials::from_json(&acc.account_json) {
            cred.email = claimed.email.clone();
            if !acc.proxy.trim().is_empty() {
                cred.proxy = acc.proxy.clone();
            }
            match crate::providers::mailbox::snapshot_baseline(&mut cred, &claimed.email).await {
                Ok((ids, at)) => {
                    let ids_json = ids.join(",");
                    let _ = pool::save_baseline(
                        pool_ref,
                        &claimed.email,
                        project_code,
                        at.naive_utc(),
                        &ids_json,
                        false,
                    )
                    .await;
                }
                Err(e) => {
                    tracing::warn!("[acquire] 抓基线失败（不阻断）email={}: {e}", claimed.email);
                }
            }
            // 预检（真实检查号活着没）
            let (alive, reason) = crate::providers::mailbox::probe_account(&mut cred, &claimed.email).await;
            if !alive {
                pool::mark_health(pool_ref, claimed.account_id, "retired", "dead", &reason).await?;
                // 换下一个号重试（简单做法：直接报无库存让客户端重试）
                return Err(AppError::bad_request("邮箱池无可用账号（预检失败）"));
            }
            // 预检可能轮换出新的 refresh_token
            let _ = pool::update_account_json(pool_ref, claimed.account_id, &cred.to_json()).await;
        }
    }

    let project = mail::get_enabled_project(pool_ref, project_code).await?;
    Ok(json!({
        "email": claimed.email,
        "provider": project.as_ref().map(|p| p.provider.clone()).unwrap_or_default(),
        "project_code": project_code,
        "project_name": project.as_ref().map(|p| p.name.clone()).unwrap_or_default(),
        "account_id": claimed.account_id,
        "status": "holding",
        "acquired_at": crate::util::utc_now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
    }))
}

// ────────────────────────────────────────────────────────────────
//  收码
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ReceiveBody {
    pub project_code: String,
    pub email: String,
    #[serde(default)]
    pub timeout: Option<f64>,
    #[serde(default)]
    pub skip_codes: Vec<String>,
}

/// `POST /api/v1/email/receive`
///
/// 轮询等待验证码。**超时不是错误**：返回 `code=00000` + `status=timeout`。
pub async fn receive_code(
    State(st): State<std::sync::Arc<AppState>>,
    identity: crate::auth::Identity,
    Json(body): Json<ReceiveBody>,
) -> AppResult<Json<Value>> {
    let pool_ref = &st.pool;
    // 同上：未知项目返回 D0003 + HTTP 200
    let Some(project) = mail::get_enabled_project(pool_ref, body.project_code.trim()).await?
    else {
        return Ok(api_err(CODE_NO_PROJECT, "项目不存在或已停用"));
    };
    let email = body.email.trim().to_ascii_lowercase();

    // 数据边界：只有「本项目买过这个邮箱」才允许收码
    if !check_owns_email(pool_ref, &identity, &email, &project.code).await? {
        return Err(AppError::forbidden("无权查看该邮箱"));
    }

    // 注入凭证 + 基线
    let Some(acc) = pool::get_account_by_email(pool_ref, &email).await? else {
        return Err(AppError::bad_request(format!("邮箱池无该邮箱的可用凭证: {email}")));
    };
    let mut cred = crate::providers::outlook::Credentials::from_json(&acc.account_json)
        .map_err(|e| AppError::bad_request(format!("凭据损坏: {e}")))?;
    if !acc.proxy.trim().is_empty() {
        cred.proxy = acc.proxy.clone();
    }
    cred.email = email.clone();
    if !cred.is_complete() {
        return Err(AppError::bad_request(format!("邮箱池无该邮箱的可用凭证: {email}")));
    }

    // 基线：没有就用 claim 时刻兜底
    let baseline = pool::get_baseline(pool_ref, &email).await?;
    let (ids, baseline_at) = match baseline {
        Some(b) => (b.message_id_set(), crate::util::naive_to_utc(b.baseline_at)),
        None => {
            let at = acc.claimed_at.map(crate::util::naive_to_utc);
            (
                std::collections::HashSet::new(),
                at.unwrap_or_else(crate::util::utc_now),
            )
        }
    };

    let timeout_secs = body.timeout.unwrap_or(120.0).clamp(10.0, 600.0);
    let opts = crate::providers::graph::WaitOptions {
        timeout: std::time::Duration::from_secs_f64(timeout_secs),
        baseline_ids: ids,
        baseline_at: Some(baseline_at),
        skip_codes: body.skip_codes.iter().cloned().collect(),
        code_regex: project.cfg_str("code_regex"),
    };

    let started = std::time::Instant::now();
    let result = crate::providers::mailbox::wait_mail(&mut cred, &email, &opts).await;

    // 凭据失效 → 判死号
    let result = match result {
        Ok(r) => r,
        Err(e) => {
            let msg = e.to_string();
            if crate::providers::outlook::is_credential_fatal(&msg) {
                pool::mark_health(pool_ref, acc.id, "retired", "dead", &msg).await?;
            }
            return Err(e);
        }
    };

    let elapsed = started.elapsed().as_secs_f64();
    // 预检可能轮换出 rt
    let _ = pool::update_account_json(pool_ref, acc.id, &cred.to_json()).await;

    match result.hit {
        Some(hit) => {
            let code = hit.code.clone();
            // 落库
            let received = crate::util::normalize_utc(&hit.mail.received_at)
                .map(|d| d.naive_utc());
            let _ = mail::save_mail_record(
                pool_ref,
                &email,
                &project.code,
                &hit.mail.message_id,
                "outlook",
                &hit.mail.folder,
                &hit.mail.sender,
                &hit.mail.subject,
                &hit.mail.body_preview,
                &hit.mail.body,
                &code,
                if hit.from_regex { "regex" } else { "rule" },
                received,
            )
            .await;

            // 收到验证码 → 真扣费（冻结转实扣）
            if let Some(uid) = identity.user_id {
                if let Ok(Some(_)) = billing::charge(
                    pool_ref,
                    uid,
                    &email,
                    "",
                    Some(&project.code),
                )
                .await
                {
                    st.stats
                        .codes
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }

            // status 语义：
            //   success      拿到验证码
            //   mail_arrived 邮件到了但提取不出码（「点击链接验证」型站点）
            let status = if code.is_empty() { "mail_arrived" } else { "success" };
            Ok(ok(json!({
                "email": email,
                "code": code,
                "status": status,
                "elapsed": (elapsed * 100.0).round() / 100.0,
                "received_at": crate::util::iso(&hit.mail.received_at),
                "mails": [crate::providers::mailbox::hit_to_api_mail(&hit)],
            })))
        }
        None => Ok(ok(json!({
            "email": email,
            "code": "",
            "status": "timeout",
            "elapsed": (elapsed * 100.0).round() / 100.0,
            "message": format!("等待 {timeout_secs:.0}s 未收到验证码"),
            "mails": [],
        }))),
    }
}

/// 数据边界校验：这个邮箱是不是「当前身份」买的。
async fn check_owns_email(
    pool_ref: &sqlx::SqlitePool,
    identity: &crate::auth::Identity,
    email: &str,
    project_code: &str,
) -> AppResult<bool> {
    // 管理员放行
    if identity.is_admin {
        return Ok(true);
    }
    // 计费关闭时无用户归属：只要有绑定记录即可（保持旧版免费行为）
    let billing_on = settings::get_billing_config(pool_ref).await?.is_enabled();
    if !billing_on {
        return Ok(pool::used_by_project(pool_ref, email, project_code).await?);
    }
    let Some(uid) = identity.user_id else {
        return Ok(false);
    };
    let order = billing::find_order(pool_ref, email, Some(project_code)).await?;
    Ok(matches!(order, Some(o) if o.user_id == uid))
}

// ────────────────────────────────────────────────────────────────
//  释放
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ReleaseBody {
    pub project_code: String,
    pub email: String,
}

/// `POST /api/v1/email/release`
///
/// 回池 + 解冻（不扣费）。
pub async fn release_email(
    State(st): State<std::sync::Arc<AppState>>,
    identity: crate::auth::Identity,
    Json(body): Json<ReleaseBody>,
) -> AppResult<Json<Value>> {
    let pool_ref = &st.pool;
    let Some(project) = mail::get_enabled_project(pool_ref, body.project_code.trim()).await?
    else {
        return Ok(api_err(CODE_NO_PROJECT, "项目不存在或已停用"));
    };
    let email = body.email.trim().to_ascii_lowercase();

    if !check_owns_email(pool_ref, &identity, &email, &project.code).await? {
        return Err(AppError::forbidden("无权操作该邮箱"));
    }

    let released = pool::release_account_by_email(pool_ref, &email).await?;

    // 解冻
    let uid = identity
        .user_id
        .or_else(|| futures_uid_placeholder());
    let _ = uid;
    if let Some(order) = billing::find_order(pool_ref, &email, Some(&project.code)).await? {
        let _ = billing::release(pool_ref, order.user_id, &email, "", Some(&project.code)).await;
    }

    Ok(ok(json!({
        "email": email,
        "released": released,
        "status": "released",
    })))
}

fn futures_uid_placeholder() -> Option<i64> {
    None
}

// ────────────────────────────────────────────────────────────────
//  邮件列表
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MailsQuery {
    pub email: Option<String>,
    pub project_code: Option<String>,
    #[serde(default)]
    pub page: i64,
    #[serde(default)]
    pub page_size: i64,
}

impl MailsQuery {
    /// 页码（默认 1）。
    fn page_no(&self) -> i64 {
        if self.page < 1 {
            1
        } else {
            self.page
        }
    }

    /// 每页条数：**默认 20**（对齐 Python），上限 200。
    ///
    /// 之前默认给 1（因为 `clamp(1,200)` 套在 0 上得到 1），
    /// 导致客户端不传 page_size 时只拿到 1 条 —— 差分测试抓到的真 bug。
    fn page_size_no(&self) -> i64 {
        if self.page_size <= 0 {
            20
        } else {
            self.page_size.min(200)
        }
    }
}

/// `GET /api/v1/mails`
pub async fn list_mails(
    State(st): State<std::sync::Arc<AppState>>,
    identity: crate::auth::Identity,
    Query(q): Query<MailsQuery>,
) -> AppResult<Json<Value>> {
    let pool_ref = &st.pool;
    let page = q.page_no();
    let page_size = q.page_size_no();
    let offset = (page - 1) * page_size;

    // 数据边界（对齐 Python `_owns_email` 的语义）：
    // - 管理员 / 未登录的匿名 key / 计费关闭 → 可以不带 email 查全部
    // - 计费开启 + 绑定了用户 → 只能查自己买过的
    //
    // 之前这里无条件要求非管理员必须带 email，会把「免费模式」的老客户端
    // 直接 403 掉（差分测试抓到的第一个真 bug）。
    let billing_on = settings::get_billing_config(pool_ref).await?.is_enabled();
    let scoped_uid = if billing_on && !identity.is_admin {
        identity.user_id
    } else {
        None
    };

    let email_filter = match (&q.email, scoped_uid) {
        // 指定了 email：校验归属
        (Some(e), Some(_uid)) => {
            let pc = q.project_code.clone().unwrap_or_default();
            if !check_owns_email(pool_ref, &identity, &e.to_ascii_lowercase(), &pc).await? {
                return Err(AppError::forbidden("无权查看该邮箱"));
            }
            Some(e.clone())
        }
        (Some(e), None) => Some(e.clone()),
        // 没指定 email：管理员/匿名放行，绑定用户只看自己的邮箱列表
        (None, Some(uid)) => {
            let emails: Vec<String> = sqlx::query_scalar(
                "SELECT DISTINCT email FROM email_orders WHERE user_id = ?",
            )
            .bind(uid)
            .fetch_all(pool_ref)
            .await?;
            if emails.is_empty() {
                // 没有订单 → 空结果（而不是报错）
                return Ok(ok(json!({
                    "total": 0,
                    "page": page,
                    "page_size": page_size,
                    "items": [],
                })));
            }
            // 用第一个邮箱占位，下面改用 emails 白名单查
            return list_mails_by_emails(pool_ref, &emails, q.project_code.as_deref(), page, page_size, offset).await;
        }
        (None, None) => None,
    };

    let total = mail::count_mail_records(pool_ref).await?;
    let records = mail::list_mail_records(
        pool_ref,
        email_filter.as_deref(),
        q.project_code.as_deref(),
        None,
        page_size,
        offset,
    )
    .await?;

    let items: Vec<Value> = records
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "email": r.email,
                "project_code": r.project_code,
                "message_id": r.message_id,
                "provider": r.provider,
                "folder": r.folder,
                "sender": r.sender,
                "subject": r.subject,
                "body_preview": r.body_preview,
                "body": r.body,
                "code": r.code,
                "code_source": r.code_source,
                "received_at": crate::util::iso_from_naive(Some(r.received_at)),
            })
        })
        .collect();

    Ok(ok(json!({"total": total, "items": items, "page": page, "page_size": page_size})))
}

/// 按邮箱白名单查（绑定用户不指定 email 时用）。
async fn list_mails_by_emails(
    pool_ref: &sqlx::SqlitePool,
    emails: &[String],
    project_code: Option<&str>,
    page: i64,
    page_size: i64,
    offset: i64,
) -> AppResult<Json<Value>> {
    let records =
        mail::list_mail_records(pool_ref, None, project_code, Some(emails), page_size, offset)
            .await?;
    let total = records.len() as i64;
    let items: Vec<Value> = records
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "email": r.email,
                "project_code": r.project_code,
                "message_id": r.message_id,
                "provider": r.provider,
                "folder": r.folder,
                "sender": r.sender,
                "subject": r.subject,
                "body_preview": r.body_preview,
                "body": r.body,
                "code": r.code,
                "code_source": r.code_source,
                "received_at": crate::util::iso_from_naive(Some(r.received_at)),
            })
        })
        .collect();
    Ok(ok(json!({"total": total, "items": items, "page": page, "page_size": page_size})))
}

// ────────────────────────────────────────────────────────────────
//  健康检查
// ────────────────────────────────────────────────────────────────

/// `GET /api/v1/health`
pub async fn health(State(st): State<std::sync::Arc<AppState>>) -> AppResult<Json<Value>> {
    let pool_ref = &st.pool;
    let projects = mail::list_enabled_projects(pool_ref).await?;

    // **stock 语义**：这里要的是「号池里可发的号有多少个」——
    // 不是「所有项目库存之和」。
    //
    // 55 个项目共用同一个 outlook 池，每个项目的 count_claimable 只差
    // 「它自己用掉了几个号」，把它们加起来会得到 969×53 = 51357 这种
    // 荒谬数字（差分测试抓到的 bug）。
    // 正确口径：单项目的可 claim 数（取第一个项目即可代表池子水位），
    // 或者更直接 —— 池子里 available+healthy 的总数。
    let stock = pool::count_claimable(pool_ref, "outlook", "").await?;

    Ok(ok(json!({
        "status": "online",
        "providers": [{
            "id": "outlook",
            "name": "Outlook 私有邮箱池",
            "pooled": true,
            "configured": true,
            "source": "email_pool",
            "stock": stock,
        }],
        "project_count": projects.len(),
    })))
}

/// `GET /api` — 接口索引（对齐 Python 版）。
pub async fn api_index() -> Json<Value> {
    Json(json!({
        "name": "MailPool",
        // 版本号与 Python 版保持一致：这是给客户端探测用的，
        // 改成 2.0.0 会让某些按版本分支的客户端走进未知路径。
        "version": "0.1.0",
        "endpoints": {
            "product_info": "GET /api/product/info",
            "buy": "POST /api/v1/email/temporary/buy  {project_code, count}",
            "receive": "POST /api/v1/email/receive  {project_code, email, timeout}",
            "release": "POST /api/v1/email/release  {project_code, email}",
            "mails": "GET /api/v1/mails?email=&project_code=&page=&page_size=",
            "health": "GET /api/v1/health",
        },
        "docs": "/docs",
    }))
}

/// 兼容未使用的 HeaderMap 导入（供后续鉴权扩展）。
#[allow(dead_code)]
fn _headers_marker(_h: &HeaderMap) {}
