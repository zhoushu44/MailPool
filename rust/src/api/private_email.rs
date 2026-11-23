//! 私有邮箱 API。
//!
//! | 方法 | 路径 | 说明 |
//! |---|---|---|
//! | POST | `/api/app/private/upload`  | 批量上传（文本行，验证后入库） |
//! | GET  | `/api/app/private/list`    | 我的私有邮箱 |
//! | POST | `/api/app/private/remove`  | 删除一条 |
//! | POST | `/api/app/private/verify`  | 重新验证一条 |
//!
//! 全部需要登录会话。

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{AppState, Identity};
use crate::error::{api_err, ok, AppError, AppResult, CODE_OP_FAILED};
use crate::services::private_email;

pub fn router() -> axum::Router<Arc<AppState>> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/api/app/private/upload", post(upload))
        .route("/api/app/private/list", get(list_mine))
        .route("/api/app/private/remove", post(remove))
        .route("/api/app/private/verify", post(verify))
}

fn need_uid(identity: &Identity) -> AppResult<i64> {
    identity
        .user_id
        .ok_or_else(|| AppError::unauthorized("需要登录"))
}

#[derive(Debug, Deserialize)]
struct UploadBody {
    /// 多行文本，每行一条（---- 分隔或 JSON）
    text: String,
}

/// `POST /api/app/private/upload`
///
/// 逐条验证（每批最多 20 条，防止一次灌几百个把 token 端点打爆）。
/// 验证失败的也入库但标 invalid，用户能看到原因。
async fn upload(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<UploadBody>,
) -> AppResult<Json<Value>> {
    let uid = need_uid(&identity)?;

    let items = private_email::parse_upload_text(&body.text);
    if items.is_empty() {
        return Ok(api_err(CODE_OP_FAILED, "没有可识别的行"));
    }
    if items.len() > 20 {
        return Ok(api_err(CODE_OP_FAILED, "一次最多 20 条（分批上传）"));
    }
    let bad_fmt = items.iter().filter(|(_, e)| !e.is_empty()).count();
    if bad_fmt == items.len() {
        // 全是格式错误，直接把行号报回去
        let msgs: Vec<String> = items.iter().map(|(_, e)| e.clone()).collect();
        return Ok(api_err(CODE_OP_FAILED, msgs.join("；")));
    }

    let http = crate::providers::outlook::build_http_client("")
        .map_err(|e| AppError::internal(format!("构建 HTTP 客户端失败: {e}")))?;

    let mut ok_items: Vec<Value> = Vec::new();
    let mut fail_items: Vec<Value> = Vec::new();

    for (item, fmt_err) in items {
        if !fmt_err.is_empty() {
            fail_items.push(json!({ "email": "", "reason": fmt_err }));
            continue;
        }
        let (email, reason, valid) =
            private_email::upsert(&st.pool, &http, uid, &item).await;
        let row = json!({
            "email": email,
            "reason": reason,
            "status": if valid { "active" } else { "invalid" },
        });
        if valid {
            ok_items.push(row);
        } else {
            fail_items.push(row);
        }
    }

    Ok(ok(json!({
        "added": ok_items.len(),
        "failed": fail_items.len(),
        "items": ok_items,
        "failures": fail_items,
    })))
}

async fn list_mine(
    State(st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    let uid = need_uid(&identity)?;
    let rows = private_email::list_mine(&st.pool, uid).await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "email": r.email,
                "status": r.status,
                "status_reason": r.status_reason,
                "created_at": crate::util::iso_from_naive(Some(r.created_at)),
                "last_check_at": crate::util::iso_from_naive(r.last_check_at),
            })
        })
        .collect();
    let active = rows.iter().filter(|r| r.status == "active").count();
    Ok(ok(json!({
        "total": items.len(),
        "active": active,
        "items": items,
    })))
}

#[derive(Debug, Deserialize)]
struct RemoveBody {
    email: String,
}

async fn remove(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<RemoveBody>,
) -> AppResult<Json<Value>> {
    let uid = need_uid(&identity)?;
    let done = private_email::remove(&st.pool, uid, &body.email).await?;
    if !done {
        return Ok(api_err(CODE_OP_FAILED, "记录不存在"));
    }
    Ok(ok(json!({ "email": body.email.trim(), "removed": true })))
}

#[derive(Debug, Deserialize)]
struct VerifyBody {
    email: String,
}

async fn verify(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<VerifyBody>,
) -> AppResult<Json<Value>> {
    let uid = need_uid(&identity)?;
    let http = crate::providers::outlook::build_http_client("")
        .map_err(|e| AppError::internal(format!("构建 HTTP 客户端失败: {e}")))?;
    let (valid, reason) =
        private_email::reverify(&st.pool, &http, uid, &body.email).await?;
    Ok(ok(json!({
        "email": body.email.trim(),
        "status": if valid { "active" } else { "invalid" },
        "reason": reason,
    })))
}
