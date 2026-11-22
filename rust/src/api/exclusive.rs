//! 独占邮箱 API。
//!
//! 端点与临时邮箱区分开，避免语义混淆：
//!
//! | 方法 | 路径 | 说明 |
//! |---|---|---|
//! | GET  | `/api/app/exclusive/list`      | 我买的独占邮箱 |
//! | POST | `/api/app/exclusive/buy`       | 买一个 |
//! | POST | `/api/app/exclusive/release`   | 释放回池 |
//! | GET  | `/api/app/exclusive/stock`     | 可售数量（前端显示「仅剩 N 个」） |

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{AppState, Identity};
use crate::error::{api_err, ok, AppError, AppResult, CODE_BALANCE, CODE_NO_STOCK, CODE_OP_FAILED};
use crate::services::exclusive;

pub fn router() -> axum::Router<Arc<AppState>> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/api/app/exclusive/list", get(list_mine))
        .route("/api/app/exclusive/buy", post(buy))
        .route("/api/app/exclusive/release", post(release))
        .route("/api/app/exclusive/stock", get(stock))
}

fn need_login(identity: &Identity) -> AppResult<i64> {
    identity
        .user_id
        .ok_or_else(|| AppError::unauthorized("需要登录"))
}

async fn list_mine(
    State(st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    let uid = need_login(&identity)?;
    let rows = exclusive::list_mine(&st.pool, uid, false).await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|e| {
            json!({
                "id": e.id,
                "email": e.email,
                "provider": e.provider,
                "price": crate::money::Money::from_milli(e.price).to_yuan(),
                "status": e.status,
                "created_at": crate::util::iso_from_naive(Some(e.created_at)),
            })
        })
        .collect();
    Ok(ok(json!({ "total": items.len(), "items": items })))
}

async fn stock(
    State(st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    // 不要求登录：前端未登录时也要能显示库存（和购买页一样）
    let _ = identity;
    let n = exclusive::count_available(&st.pool, "outlook").await?;
    Ok(ok(json!({ "available": n })))
}

#[derive(Debug, Deserialize)]
struct BuyBody {
    /// 买几个（默认 1，上限 10 —— 独占邮箱贵，防止手滑）
    #[serde(default)]
    count: Option<i64>,
}

async fn buy(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<BuyBody>,
) -> AppResult<Json<Value>> {
    let uid = need_login(&identity)?;
    let count = body.count.unwrap_or(1).clamp(1, 10);

    let billing_cfg = crate::services::settings::get_billing_config(&st.pool).await?;
    let billing_on = billing_cfg.is_enabled();

    // 独占邮箱的单价：目前从设置里读 `exclusive.price`（厘），
    // 没有配置就当免费（和临时邮箱在未计费时的口径一致）。
    let unit_price: i64 = crate::services::settings::get_value(
        &st.pool,
        "exclusive.price",
        "0",
    )
    .await?
    .parse()
    .unwrap_or(0);

    if billing_on && unit_price > 0 {
        if let Some(u) = crate::services::billing::get_user(&st.pool, uid).await? {
            let need = unit_price * count;
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

    let mut items = Vec::new();
    for _ in 0..count {
        match exclusive::claim_exclusive(&st.pool, uid, "outlook", unit_price).await {
            Ok(Some(e)) => {
                // 计费开启且有价格 → 真扣（独占是一次性买断，不走冻结）
                if billing_on && unit_price > 0 {
                    if let Err(err) = exclusive::charge_once(
                        &st.pool, uid, unit_price,
                        &format!("购买独占邮箱 {}", e.email),
                    )
                    .await
                    {
                        // 扣费失败 → 把刚占的号退回池子，别让用户白占
                        let _ = exclusive::release_exclusive(&st.pool, uid, &e.email).await;
                        return Ok(api_err(CODE_BALANCE, format!("扣费失败: {err}")));
                    }
                }
                items.push(json!({
                    "id": e.id,
                    "email": e.email,
                    "provider": e.provider,
                    "status": e.status,
                    "price": crate::money::Money::from_milli(unit_price).to_yuan(),
                    "created_at": crate::util::iso_from_naive(Some(e.created_at)),
                }));
            }
            Ok(None) => {
                if items.is_empty() {
                    return Ok(api_err(CODE_NO_STOCK, "独占邮箱暂时售罄"));
                }
                break;
            }
            Err(e) => {
                if items.is_empty() {
                    return Ok(api_err(CODE_OP_FAILED, format!("购买失败: {e}")));
                }
                break;
            }
        }
    }

    Ok(ok(Value::Array(items)))
}

#[derive(Debug, Deserialize)]
struct ReleaseBody {
    email: String,
}

async fn release(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<ReleaseBody>,
) -> AppResult<Json<Value>> {
    let uid = need_login(&identity)?;
    let email = body.email.trim();
    if email.is_empty() {
        return Ok(api_err(CODE_OP_FAILED, "缺少 email"));
    }

    let ok_released = exclusive::release_exclusive(&st.pool, uid, email).await?;
    if !ok_released {
        return Ok(api_err(CODE_OP_FAILED, "这个邮箱不在你的独占列表里"));
    }

    Ok(ok(json!({ "email": email, "released": true })))
}
