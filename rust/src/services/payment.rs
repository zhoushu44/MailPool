//! 充值支付：易支付网关对接 + 充值卡。
//!
//! ## 易支付签名（最容易写错的地方）
//!
//! ```text
//! 1. 过滤空值，过滤 sign / sign_type 字段
//! 2. 按 key **字典序**排序
//! 3. 拼成 k1=v1&k2=v2&...&key=<商户密钥>
//! 4. MD5，**小写十六进制**
//! 5. **不 urlencode**（值里直接是原文）
//! ```
//!
//! 注意最后一段是 `&key=KEY` 而不是再拼一个参数 —— 写成
//! `...&sign=xxx` 或对值做 urlencode 都会验签失败，而网关只返回
//! 「签名错误」，极难排查。

use std::collections::HashMap;

use serde_json::Value;
use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};
use crate::models::*;
use crate::money::Money;
use crate::services::billing;
use crate::services::settings::{self, BillingConfig};
use crate::util::{generate_card_key, now_naive, random_hex, truncate_chars};

/// 支持的支付方式
pub const PAY_TYPES: &[(&str, &str)] = &[("alipay", "支付宝"), ("wxpay", "微信支付")];

/// 支付错误（对用户可见的提示）。
#[derive(Debug, thiserror::Error)]
pub enum PaymentError {
    #[error("{0}")]
    Msg(String),
}

impl PaymentError {
    fn text(&self) -> String {
        match self {
            PaymentError::Msg(m) => m.clone(),
        }
    }
}

impl From<PaymentError> for AppError {
    fn from(e: PaymentError) -> Self {
        AppError::BadRequest(e.text())
    }
}

fn md5_hex(data: &[u8]) -> String {
    // 用 md-5 crate（包名 md-5，crate 名 md5）：Digest trait 提供 update/finalize
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(data);
    hex::encode(h.finalize())
}

/// 易支付签名。
///
/// 过滤空值与 sign/sign_type，按 key 字典序拼 `k=v&...&key=KEY`，MD5 小写。
pub fn sign_params(params: &[(String, String)], key: &str) -> String {
    let mut items: Vec<(String, String)> = params
        .iter()
        .filter(|(k, v)| k != "sign" && k != "sign_type" && !v.is_empty())
        .cloned()
        .collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));

    let joined = items
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let raw = format!("{joined}&key={key}");
    md5_hex(raw.as_bytes())
}

/// 校验回调签名（常量时间比较，防时序侧信道）。
pub fn verify_params(params: &[(String, String)], key: &str) -> bool {
    let given = params
        .iter()
        .find(|(k, _)| k == "sign")
        .map(|(_, v)| v.to_ascii_lowercase())
        .unwrap_or_default();
    if given.is_empty() {
        return false;
    }
    let expected = sign_params(params, key);
    crate::util::constant_time_eq(&given, &expected)
}

/// 支付通道列表（页面展示用）。
pub fn list_channels(cfg: &BillingConfig) -> Vec<Value> {
    let mut out = Vec::new();
    let gateway = cfg.epay_url.trim();
    let pid = cfg.epay_pid.trim();
    let key = cfg.epay_key.trim();
    if !gateway.is_empty() && !pid.is_empty() && !key.is_empty() {
        for (id, name) in PAY_TYPES {
            out.push(serde_json::json!({
                "name": id,
                "display_name": name,
                "category": "epay",
            }));
        }
    }
    out
}

/// 创建充值订单（向网关下单）。
pub struct CreatedOrder {
    pub id: i64,
    pub trade_no: String,
    pub url: String,
    pub params: Vec<(String, String)>,
}

/// 创建充值订单。
pub async fn create_recharge_order(
    pool: &SqlitePool,
    user_id: i64,
    amount: Money,
    pay_type: &str,
    client_ip: &str,
) -> AppResult<CreatedOrder> {
    let cfg = settings::get_billing_config(pool).await?;

    let min_topup = Money::parse_yuan(&cfg.min_topup).unwrap_or(Money::from_milli(1000));
    let max_topup = Money::parse_yuan(&cfg.max_topup).unwrap_or(Money::from_milli(1_000_000));

    if amount < min_topup {
        return Err(PaymentError::Msg(format!(
            "充值金额不能低于 {} 元",
            min_topup.to_yuan()
        ))
        .into());
    }
    if amount > max_topup {
        return Err(PaymentError::Msg(format!(
            "单次充值金额不能超过 {} 元",
            max_topup.to_yuan()
        ))
        .into());
    }
    if !PAY_TYPES.iter().any(|(id, _)| *id == pay_type) {
        return Err(PaymentError::Msg("支付方式不存在".into()).into());
    }
    // 金额最多 2 位小数（对齐 MailNest 前端 /^\d+(\.\d{1,2})?$/）
    if amount.milli() % 10 != 0 {
        return Err(PaymentError::Msg("充值金额最多支持 2 位小数".into()).into());
    }

    let Some(user) = billing::get_user(pool, user_id).await? else {
        return Err(PaymentError::Msg("用户不存在".into()).into());
    };
    if !user.is_active() {
        return Err(PaymentError::Msg("账号已被禁用".into()).into());
    }

    let gateway = cfg.epay_url.trim().trim_end_matches('/').to_string();
    let pid = cfg.epay_pid.trim().to_string();
    let key = cfg.epay_key.trim().to_string();
    if gateway.is_empty() || pid.is_empty() || key.is_empty() {
        return Err(PaymentError::Msg("未配置支付网关".into()).into());
    }

    // 订单号：带用户 id 便于排查（对齐 new-api 的 USR{id}NO{ts} 风格）
    let trade_no = format!(
        "MP{user_id}NO{}{}",
        crate::util::utc_now().timestamp(),
        random_hex(3)
    );
    let money_str = amount.to_yuan();

    let base_url = cfg.site_url.trim().trim_end_matches('/').to_string();
    let notify_url = format!("{base_url}/api/v1/recharge/notify");
    let return_url = format!("{base_url}/recharges");

    let mut params: Vec<(String, String)> = vec![
        ("pid".into(), pid),
        ("type".into(), pay_type.to_string()),
        ("out_trade_no".into(), trade_no.clone()),
        ("notify_url".into(), notify_url),
        ("return_url".into(), return_url),
        ("name".into(), format!("MailPool 充值 {money_str}")),
        ("money".into(), money_str),
    ];
    let sign = sign_params(&params, &key);
    params.push(("sign".into(), sign));
    params.push(("sign_type".into(), "MD5".into()));

    let res = sqlx::query(
        "INSERT INTO recharge_orders (user_id, trade_no, amount, money, pay_type, pay_provider, \
         status, client_ip, remark, created_at) VALUES (?, ?, ?, ?, ?, 'epay', ?, ?, '易支付', ?)",
    )
    .bind(user_id)
    .bind(&trade_no)
    .bind(amount.milli())
    .bind(amount.milli())
    .bind(pay_type)
    .bind(truncate_chars(client_ip, 64))
    .bind(TOPUP_PENDING)
    .bind(now_naive())
    .execute(pool)
    .await?;

    Ok(CreatedOrder {
        id: res.last_insert_rowid(),
        trade_no,
        url: format!("{gateway}/submit.php"),
        params,
    })
}

/// 处理支付回调（幂等）。
///
/// 返回 `(success, message)`。**签名错、金额不符、订单不存在都要拒绝**，
/// 否则任何人构造一个回调就能给自己加钱。
pub async fn process_notify(
    pool: &SqlitePool,
    params: &[(String, String)],
) -> AppResult<(bool, String)> {
    let cfg = settings::get_billing_config(pool).await?;
    let key = cfg.epay_key.trim().to_string();
    if key.is_empty() {
        return Ok((false, "未配置支付密钥".into()));
    }
    if !verify_params(params, &key) {
        return Ok((false, "签名错误".into()));
    }

    let get = |name: &str| -> String {
        params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };

    let trade_no = get("out_trade_no");
    if trade_no.is_empty() {
        return Ok((false, "缺少订单号".into()));
    }
    let trade_status = get("trade_status");
    // 易支付：TRADE_SUCCESS / TRADE_FINISHED 都算成功
    if trade_status != "TRADE_SUCCESS" && trade_status != "TRADE_FINISHED" {
        return Ok((false, format!("交易状态非成功: {trade_status}")));
    }

    let Some(order) = sqlx::query_as::<_, RechargeOrder>(
        "SELECT * FROM recharge_orders WHERE trade_no = ?",
    )
    .bind(&trade_no)
    .fetch_optional(pool)
    .await?
    else {
        return Ok((false, "订单不存在".into()));
    };

    // 幂等：已成功的订单不重复加钱
    if order.status == TOPUP_SUCCESS {
        return Ok((true, "订单已处理".into()));
    }

    // 金额校验：回调里的 money 必须与订单一致
    let paid = Money::parse_yuan(&get("money")).unwrap_or_default();
    if paid.milli() != order.money {
        return Ok((
            false,
            format!(
                "金额不符: 回调 {} 订单 {}",
                paid.to_yuan(),
                Money::from_milli(order.money).to_yuan()
            ),
        ));
    }

    // 加钱（唯一索引 trade_no + 状态检查保证幂等）
    let updated = sqlx::query(
        "UPDATE recharge_orders SET status = ?, completed_at = ? \
         WHERE trade_no = ? AND status = ?",
    )
    .bind(TOPUP_SUCCESS)
    .bind(now_naive())
    .bind(&trade_no)
    .bind(TOPUP_PENDING)
    .execute(pool)
    .await?;

    if updated.rows_affected() == 0 {
        // 并发下另一个请求已经处理了
        return Ok((true, "订单已处理".into()));
    }

    billing::credit(
        pool,
        order.user_id,
        Money::from_milli(order.amount),
        LOG_RECHARGE,
        &format!("扫码充值 {trade_no}"),
        Some(true),
    )
    .await?;

    Ok((true, "success".into()))
}

/// 管理员补单（人工确认到账）。
pub async fn admin_complete_topup(
    pool: &SqlitePool,
    trade_no: &str,
    client_ip: &str,
) -> AppResult<(bool, String)> {
    let Some(order) = sqlx::query_as::<_, RechargeOrder>(
        "SELECT * FROM recharge_orders WHERE trade_no = ?",
    )
    .bind(trade_no)
    .fetch_optional(pool)
    .await?
    else {
        return Ok((false, "订单不存在".into()));
    };
    if order.status == TOPUP_SUCCESS {
        return Ok((false, "订单已是成功状态".into()));
    }

    let updated = sqlx::query(
        "UPDATE recharge_orders SET status = ?, completed_at = ?, \
         remark = COALESCE(NULLIF(remark,''), '人工补单') WHERE trade_no = ? AND status <> ?",
    )
    .bind(TOPUP_SUCCESS)
    .bind(now_naive())
    .bind(trade_no)
    .bind(TOPUP_SUCCESS)
    .execute(pool)
    .await?;
    if updated.rows_affected() == 0 {
        return Ok((false, "订单状态已变更".into()));
    }

    billing::credit(
        pool,
        order.user_id,
        Money::from_milli(order.amount),
        LOG_RECHARGE,
        &format!("人工补单 {trade_no} {client_ip}"),
        Some(true),
    )
    .await?;
    Ok((true, "补单成功".into()))
}

/// 标记订单失败（不加钱）。
pub async fn admin_fail_topup(pool: &SqlitePool, trade_no: &str) -> AppResult<(bool, String)> {
    let res = sqlx::query(
        "UPDATE recharge_orders SET status = ? WHERE trade_no = ? AND status = ?",
    )
    .bind(TOPUP_FAILED)
    .bind(trade_no)
    .bind(TOPUP_PENDING)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Ok((false, "订单不存在或状态不可变更".into()));
    }
    Ok((true, "已标记失败".into()))
}

// ────────────────────────────────────────────────────────────────
//  充值卡
// ────────────────────────────────────────────────────────────────

/// 批量生成充值卡。返回 `(batch_id, cards)`。
pub async fn generate_cards(
    pool: &SqlitePool,
    amount: Money,
    count: u32,
    batch_id: &str,
) -> AppResult<(String, Vec<String>)> {
    let batch = if batch_id.trim().is_empty() {
        format!("B{}", random_hex(6))
    } else {
        batch_id.trim().to_string()
    };

    let mut cards = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let key = generate_card_key();
        sqlx::query(
            "INSERT INTO redeem_cards (batch_id, card_key, amount, status, used_by, created_at) \
             VALUES (?, ?, ?, 0, 0, ?)",
        )
        .bind(&batch)
        .bind(&key)
        .bind(amount.milli())
        .bind(now_naive())
        .execute(pool)
        .await?;
        cards.push(key);
    }
    Ok((batch, cards))
}

/// 查询充值卡信息。
///
/// **status 语义修正**：Python 版写的是
/// `"未使用" if card.status == 1 else "已使用"` —— 反了。
/// 建卡时 `status=0`（未使用），兑换后置 1（已使用），所以正确映射是
/// `0 → 未使用`、`1 → 已使用`。这里按正确语义输出，
/// 并在字段里同时给出 `used` 布尔值，让前端不依赖中文。
pub async fn query_card(pool: &SqlitePool, card_key: &str) -> AppResult<Option<Value>> {
    let row = sqlx::query_as::<_, RedeemCard>(
        "SELECT * FROM redeem_cards WHERE card_key = ?",
    )
    .bind(card_key.trim())
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|c| {
        let used = c.status != 0;
        serde_json::json!({
            "batch_id": c.batch_id,
            "card_key": c.card_key,
            "amount": Money::from_milli(c.amount).to_yuan(),
            "status": if used { "已使用" } else { "未使用" },
            "used": used,
            "used_by": if c.used_by == 0 { Value::Null } else { serde_json::json!(c.used_by) },
            "used_at": crate::util::iso_from_naive(c.used_at),
            "created_at": crate::util::iso_from_naive(Some(c.created_at)),
        })
    }))
}

/// 兑换充值卡。返回 `(ok, message)`。
pub async fn redeem_card(
    pool: &SqlitePool,
    user_id: i64,
    card_key: &str,
) -> AppResult<(bool, String)> {
    let key = card_key.trim();
    if key.is_empty() {
        return Ok((false, "请提供卡密".into()));
    }

    // 原子占用：只有 status=0 的卡能被抢到，并发兑换不会重复充值
    let res = sqlx::query(
        "UPDATE redeem_cards SET status = 1, used_by = ?, used_at = ? \
         WHERE card_key = ? AND status = 0",
    )
    .bind(user_id)
    .bind(now_naive())
    .bind(key)
    .execute(pool)
    .await?;

    if res.rows_affected() == 0 {
        // 分清「不存在」和「已用过」
        let exists: Option<i64> =
            sqlx::query_scalar("SELECT id FROM redeem_cards WHERE card_key = ?")
                .bind(key)
                .fetch_optional(pool)
                .await?;
        return Ok((
            false,
            if exists.is_some() {
                "卡密已被使用".into()
            } else {
                "卡密不存在".into()
            },
        ));
    }

    let amount: i64 = sqlx::query_scalar("SELECT amount FROM redeem_cards WHERE card_key = ?")
        .bind(key)
        .fetch_one(pool)
        .await?;

    billing::credit(
        pool,
        user_id,
        Money::from_milli(amount),
        LOG_REDEEM,
        &format!("充值卡兑换 {key}"),
        Some(true),
    )
    .await?;

    Ok((true, format!("兑换成功，到账 {} 元", Money::from_milli(amount).to_yuan())))
}

/// 批量查询卡密详情（按批次）。
pub async fn list_cards_by_batch(
    pool: &SqlitePool,
    batch_id: &str,
) -> AppResult<Vec<Value>> {
    let rows = sqlx::query_as::<_, RedeemCard>(
        "SELECT * FROM redeem_cards WHERE batch_id = ? ORDER BY id ASC",
    )
    .bind(batch_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|c| {
            let used = c.status != 0;
            serde_json::json!({
                "card_key": c.card_key,
                "amount": Money::from_milli(c.amount).to_yuan(),
                "status": if used { "已使用" } else { "未使用" },
                "used": used,
                "used_by": c.used_by,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_is_md5_lowercase() {
        // 手算：a=1&b=2&key=SECRET → md5("a=1&b=2&key=SECRET")
        let params = vec![
            ("b".to_string(), "2".to_string()),
            ("a".to_string(), "1".to_string()),
        ];
        let sig = sign_params(&params, "SECRET");
        assert_eq!(sig.len(), 32);
        assert_eq!(sig, sig.to_ascii_lowercase(), "必须是 MD5 小写");

        let expected = md5_hex(b"a=1&b=2&key=SECRET");
        assert_eq!(sig, expected, "必须按 key 字典序拼接");
    }

    #[test]
    fn sign_filters_empty_and_sign_fields() {
        let params = vec![
            ("a".to_string(), "1".to_string()),
            ("empty".to_string(), "".to_string()),
            ("sign".to_string(), "xxx".to_string()),
            ("sign_type".to_string(), "MD5".to_string()),
        ];
        let sig = sign_params(&params, "KEY");
        // 只应包含 a=1
        assert_eq!(sig, md5_hex(b"a=1&key=KEY"));
    }

    #[test]
    fn sign_does_not_urlencode() {
        // 值里有中文/空格时不能编码
        let params = vec![("name".to_string(), "MailPool 充值 1.000".to_string())];
        let sig = sign_params(&params, "K");
        let expected = md5_hex("name=MailPool 充值 1.000&key=K".as_bytes());
        assert_eq!(sig, expected);
    }

    #[test]
    fn verify_accepts_correct_sign() {
        let mut params = vec![
            ("out_trade_no".to_string(), "T1".to_string()),
            ("money".to_string(), "1.00".to_string()),
        ];
        let sig = sign_params(&params, "KEY");
        params.push(("sign".to_string(), sig));
        assert!(verify_params(&params, "KEY"));
    }

    #[test]
    fn verify_rejects_tampered() {
        let params = vec![
            ("out_trade_no".to_string(), "T1".to_string()),
            ("money".to_string(), "1.00".to_string()),
            ("sign".to_string(), "deadbeef".to_string()),
        ];
        assert!(!verify_params(&params, "KEY"));
    }

    #[test]
    fn verify_rejects_missing_sign() {
        let params = vec![("money".to_string(), "1.00".to_string())];
        assert!(!verify_params(&params, "KEY"));
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let mut params = vec![("money".to_string(), "1.00".to_string())];
        let sig = sign_params(&params, "KEY");
        params.push(("sign".to_string(), sig));
        assert!(!verify_params(&params, "WRONG"));
    }

    #[test]
    fn channels_hidden_without_config() {
        let cfg = BillingConfig::default(); // epay 全空
        assert!(list_channels(&cfg).is_empty());
    }

    #[test]
    fn channels_visible_with_config() {
        let cfg = BillingConfig {
            epay_url: "https://pay.test".into(),
            epay_pid: "1001".into(),
            epay_key: "k".into(),
            ..Default::default()
        };
        let ch = list_channels(&cfg);
        assert_eq!(ch.len(), 2);
        assert_eq!(ch[0]["name"], "alipay");
        assert_eq!(ch[1]["name"], "wxpay");
    }
}
