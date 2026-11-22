//! 充值 API：`/api/v1/recharge*`。
//!
//! 路径与 Python `app/api/recharge_api.py` **逐字对齐**：
//!
//! | 方法 | 路径 | 说明 |
//! |---|---|---|
//! | GET  | `/api/v1/recharge`            | 余额六栏 |
//! | GET  | `/api/v1/recharge/channels`   | 支付通道 |
//! | POST | `/api/v1/recharge/create`     | 下单 |
//! | GET  | `/api/v1/recharge/query`      | 查订单状态 |
//! | GET  | `/api/v1/recharge/orders`     | 充值订单列表 |
//! | POST | `/api/v1/recharge/redeem-card`| 兑换充值卡 |
//! | POST | `/api/v1/recharge/notify`     | 支付回调（公开） |
//!
//! 注意没有 `/api/v1/balance` —— 余额在 `/api/v1/recharge` 里返回。
//! 我一开始按 new-api 的习惯写成了 `/api/v1/balance`，差分测试直接抓到 404。

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::AppState;
use crate::error::*;
use crate::money::Money;
use crate::services::{billing, payment, settings};

/// 充值路由。
pub fn router() -> axum::Router<std::sync::Arc<AppState>> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/api/v1/recharge", get(balance))
        .route("/api/v1/recharge/channels", get(channels))
        .route("/api/v1/recharge/create", post(create_order))
        .route("/api/v1/recharge/query", get(order_status))
        .route("/api/v1/recharge/orders", get(order_list))
        .route("/api/v1/recharge/redeem-card", post(redeem))
        .route("/api/v1/recharge/notify", post(notify_post).get(notify_get))
}

/// 取当前用户 ID；未登录返回 401（充值必须绑定真实用户）。
fn require_uid(identity: &crate::auth::Identity) -> AppResult<i64> {
    identity.user_id.ok_or_else(|| {
        AppError::unauthorized("需要有效的用户身份（API Key 绑定用户或网页登录）")
    })
}

/// `GET /api/v1/recharge` — 余额六栏。
pub async fn balance(
    State(st): State<std::sync::Arc<AppState>>,
    identity: crate::auth::Identity,
) -> AppResult<Json<Value>> {
    let uid = require_uid(&identity)?;
    let snap = billing::balance_snapshot(&st.pool, uid).await?;
    Ok(ok(serde_json::to_value(snap)?))
}

/// `GET /api/v1/recharge/channels` — 支付通道。
///
/// **只返回 `{"channels":[...]}`**，不带 min/max —— Python 版就是这样，
/// 前端从 `/api/v1/recharge` 的配置里读限额。多返回字段会让差分测试报
/// `Missing`（Rust 有 Python 没有），而客户端契约以 Python 为准。
pub async fn channels(
    State(st): State<std::sync::Arc<AppState>>,
    _identity: crate::auth::Identity,
) -> AppResult<Json<Value>> {
    let cfg = settings::get_billing_config(&st.pool).await?;
    let ch = payment::list_channels(&cfg);
    Ok(ok(json!({"channels": ch})))
}

#[derive(Debug, Deserialize)]
pub struct CreateOrderBody {
    /// 金额（元，字符串）
    pub amount: String,
    #[serde(default = "default_pay_type")]
    pub pay_type: String,
}

fn default_pay_type() -> String {
    "alipay".to_string()
}

/// `POST /api/v1/recharge/create` — 充值下单。
pub async fn create_order(
    State(st): State<std::sync::Arc<AppState>>,
    identity: crate::auth::Identity,
    Json(body): Json<CreateOrderBody>,
) -> AppResult<Json<Value>> {
    let uid = require_uid(&identity)?;
    let Some(amount) = Money::parse_yuan(&body.amount) else {
        return Ok(api_err(CODE_PAYMENT, "金额格式不正确"));
    };

    match payment::create_recharge_order(
        &st.pool,
        uid,
        amount,
        &body.pay_type,
        &identity.client_ip,
    )
    .await
    {
        Ok(o) => {
            let params: serde_json::Map<String, Value> = o
                .params
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            Ok(ok(json!({
                "id": o.id,
                "trade_no": o.trade_no,
                "url": o.url,
                "params": params,
            })))
        }
        // 业务失败用 D0007 + HTTP 200（客户端只看 code）
        Err(AppError::BadRequest(msg)) => Ok(api_err(CODE_PAYMENT, msg)),
        Err(e) => Err(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct QueryBody {
    #[serde(default)]
    pub trade_no: String,
}

/// `GET /api/v1/recharge/query` — 查订单状态。
pub async fn order_status(
    State(st): State<std::sync::Arc<AppState>>,
    identity: crate::auth::Identity,
    Query(q): Query<QueryBody>,
) -> AppResult<Json<Value>> {
    let uid = require_uid(&identity)?;
    let row = sqlx::query_as::<_, crate::models::RechargeOrder>(
        "SELECT * FROM recharge_orders WHERE trade_no = ?",
    )
    .bind(q.trade_no.trim())
    .fetch_optional(&st.pool)
    .await?;

    let Some(o) = row else {
        return Ok(api_err(CODE_PAYMENT, "订单不存在"));
    };
    if o.user_id != uid && !identity.is_admin {
        return Err(AppError::forbidden("无权查看该订单"));
    }

    Ok(ok(json!({
        "trade_no": o.trade_no,
        "status": o.status,
        "amount": Money::from_milli(o.amount).to_yuan(),
        "money": Money::from_milli(o.money).to_yuan(),
        "created_at": crate::util::iso_from_naive(Some(o.created_at)),
        "completed_at": crate::util::iso_from_naive(o.completed_at),
    })))
}

#[derive(Debug, Deserialize)]
pub struct OrdersQuery {
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub page_size: Option<i64>,
}

/// `GET /api/v1/recharge/orders` — 充值订单列表。
pub async fn order_list(
    State(st): State<std::sync::Arc<AppState>>,
    identity: crate::auth::Identity,
    Query(q): Query<OrdersQuery>,
) -> AppResult<Json<Value>> {
    let uid = require_uid(&identity)?;
    let page = q.page.unwrap_or(1).max(1);
    let page_size = q.page_size.unwrap_or(20).clamp(1, 100);
    let scope = if identity.is_admin { None } else { Some(uid) };
    let (orders, total) =
        billing::list_recharge_orders(&st.pool, scope, page_size, (page - 1) * page_size).await?;

    Ok(ok(json!({
        "total": total,
        "page": page,
        "page_size": page_size,
        "items": orders.iter().map(|o| json!({
            "id": o.id,
            "trade_no": o.trade_no,
            "amount": Money::from_milli(o.amount).to_yuan(),
            "money": Money::from_milli(o.money).to_yuan(),
            "pay_type": o.pay_type,
            "status": o.status,
            "created_at": crate::util::iso_from_naive(Some(o.created_at)),
            "completed_at": crate::util::iso_from_naive(o.completed_at),
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize)]
pub struct RedeemBody {
    #[serde(default, alias = "card_no", alias = "card_key")]
    pub card: String,
}

/// `POST /api/v1/recharge/redeem-card` — 兑换充值卡。
pub async fn redeem(
    State(st): State<std::sync::Arc<AppState>>,
    identity: crate::auth::Identity,
    Json(body): Json<RedeemBody>,
) -> AppResult<Json<Value>> {
    let uid = require_uid(&identity)?;
    let (ok_flag, msg) = payment::redeem_card(&st.pool, uid, &body.card).await?;
    if ok_flag {
        Ok(ok(json!({"success": true, "message": msg})))
    } else {
        Ok(api_err(CODE_PAYMENT, msg))
    }
}

async fn notify_post(
    State(st): State<std::sync::Arc<AppState>>,
    axum::extract::Form(form): axum::extract::Form<std::collections::HashMap<String, String>>,
) -> String {
    handle_notify(&st, &form.into_iter().collect::<Vec<_>>()).await
}

async fn notify_get(
    State(st): State<std::sync::Arc<AppState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> String {
    handle_notify(&st, &q.into_iter().collect::<Vec<_>>()).await
}

/// 回调处理：易支付要求返回纯文本 `success` 才算收到，否则会重试。
async fn handle_notify(state: &AppState, params: &[(String, String)]) -> String {
    match payment::process_notify(&state.pool, params).await {
        Ok((true, _)) => "success".to_string(),
        Ok((false, msg)) => {
            tracing::warn!("[notify] 拒绝: {msg}");
            "fail".to_string()
        }
        Err(e) => {
            tracing::error!("[notify] 处理异常: {e}");
            "fail".to_string()
        }
    }
}
