//! 收件箱：Graph 取邮件 + AI 识别验证码。
//!
//! 对齐 Python `app/api/admin_api.py` 的 `/mailbox/*` 与
//! `app/services/code_service.py`。
//!
//! ## 三个端点
//!
//! - `GET  /api/admin/mailbox/messages`  列某邮箱某文件夹的信
//! - `GET  /api/admin/mailbox/message`   取单封全文
//! - `POST /api/admin/mailbox/ai-code`   规则 + 可选 LLM 识别验证码
//!
//! ## AI 识别是可选的
//!
//! 只在同时配了 `MAILPOOL_AI_BASE_URL` / `MAILPOOL_AI_API_KEY` /
//! `MAILPOOL_AI_MODEL` 三个环境变量时才启用；否则**静默降级为纯规则**。
//! 这是刻意的：AI 不可用不该让收件箱打不开。
//!
//! 任何 AI 侧的异常（超时、JSON 解析失败、模型返回垃圾）都回退到规则结果，
//! 绝不把错误抛给页面。

use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};

/// AI 识别的返回结构（对齐 Python `extract_ai`）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AiCodeResult {
    pub verification_code: String,
    pub confidence: f64,
    pub reason: String,
}

/// 纯规则识别（无 AI）。
///
/// 优先用预提取字段（`code` / `otp` 等），没有再扫正文。
pub fn extract_rule(mail: &Value) -> String {
    let by_mail = crate::providers::extract_code_from_mail(mail);
    if !by_mail.is_empty() {
        return by_mail;
    }
    let mut combined = String::new();
    for key in ["subject", "body_preview", "body", "text"] {
        if let Some(s) = mail.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                combined.push_str(s);
                combined.push('\n');
            }
        }
    }
    crate::providers::extract_code(&combined)
}

/// 规则 + 可选 AI 识别验证码。
pub async fn extract_ai(http: &reqwest::Client, mail: &Value, use_ai: bool) -> AiCodeResult {
    let rule_code = extract_rule(mail);
    let empty = AiCodeResult {
        verification_code: rule_code.clone(),
        confidence: if rule_code.is_empty() { 0.0 } else { 1.0 },
        reason: if rule_code.is_empty() {
            String::new()
        } else {
            "rule".to_string()
        },
    };

    if !use_ai {
        return empty;
    }
    let base_url = std::env::var("MAILPOOL_AI_BASE_URL").unwrap_or_default();
    let api_key = std::env::var("MAILPOOL_AI_API_KEY").unwrap_or_default();
    let model = std::env::var("MAILPOOL_AI_MODEL").unwrap_or_default();
    if base_url.trim().is_empty() || api_key.trim().is_empty() || model.trim().is_empty() {
        return empty;
    }

    // 只发前 12000 字符：验证码邮件正文很少超过这个量，
    // 而超长正文会让 token 费用爆炸（有的营销邮件一整页 HTML）
    let mail_text = serde_json::to_string(mail).unwrap_or_default();
    let truncated: String = mail_text.chars().take(12000).collect();
    let prompt = format!(
        "Extract the verification code from this email. \
         Return JSON with verification_code, confidence, reason.\n{truncated}"
    );

    let endpoint = {
        let b = base_url.trim().trim_end_matches('/');
        if b.ends_with("/v1") {
            format!("{b}/chat/completions")
        } else {
            format!("{b}/v1/chat/completions")
        }
    };

    let payload = json!({
        "model": model.trim(),
        "messages": [{"role": "user", "content": prompt}],
        "temperature": 0,
    });

    let resp = http
        .post(&endpoint)
        .bearer_auth(api_key.trim())
        .timeout(std::time::Duration::from_secs(20))
        .json(&payload)
        .send()
        .await;

    let Ok(resp) = resp else {
        tracing::warn!("[ai-code] 请求失败，回退规则结果");
        return empty;
    };
    if !resp.status().is_success() {
        tracing::warn!("[ai-code] HTTP {}，回退规则", resp.status());
        return empty;
    }
    let Ok(body) = resp.json::<Value>().await else {
        return fallback_log("响应非 JSON", empty);
    };
    let Some(content) = body
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
    else {
        return fallback_log("响应缺 choices[0].message.content", empty);
    };

    // 模型经常把 JSON 包在 ```json ... ``` 里
    let cleaned = content
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let Ok(parsed) = serde_json::from_str::<Value>(cleaned) else {
        return fallback_log("模型输出不是合法 JSON", empty);
    };

    let code = parsed
        .get("verification_code")
        .and_then(|v| {
            if let Some(s) = v.as_str() {
                Some(s.to_string())
            } else if v.is_null() {
                None
            } else {
                Some(v.to_string())
            }
        })
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| rule_code.clone());

    AiCodeResult {
        verification_code: code,
        confidence: parsed
            .get("confidence")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        reason: parsed
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    }
}

fn fallback_log(reason: &str, empty: AiCodeResult) -> AiCodeResult {
    tracing::warn!("[ai-code] {reason}，回退规则结果");
    empty
}

/// 取某邮箱凭证（含代理）并构造 HTTP 客户端。
async fn cred_for(pool: &SqlitePool, email: &str) -> AppResult<(crate::providers::outlook::Credentials, reqwest::Client)> {
    let Some(acc) = crate::services::pool::get_account_by_email(pool, email).await? else {
        return Err(AppError::not_found("Outlook 账号不存在"));
    };
    if acc.provider != "outlook" {
        return Err(AppError::not_found("该账号不是 Outlook 号池账号"));
    }
    let mut cred = crate::providers::outlook::Credentials::from_json(&acc.account_json)
        .map_err(|e| AppError::bad_request(format!("凭据损坏: {e}")))?;
    cred.email = acc.email.clone();
    if !acc.proxy.trim().is_empty() {
        cred.proxy = acc.proxy.clone();
    }
    if !cred.is_complete() {
        return Err(AppError::bad_request("该号缺少 client_id / refresh_token"));
    }
    let http = crate::providers::outlook::build_http_client(&cred.proxy)?;
    Ok((cred, http))
}

/// 列邮件。
///
/// 返回 Graph 的原始 message 数组（页面直接读 `subject` / `from` /
/// `receivedDateTime` / `bodyPreview`），**不做二次加工** —— 加工过就
/// 和 Python 版字段对不上了。
pub async fn list_messages(
    pool: &SqlitePool,
    email: &str,
    folder: &str,
    top: u32,
) -> AppResult<Value> {
    let (mut cred, http) = cred_for(pool, email).await?;
    let token = crate::providers::outlook::access_token(
        &http,
        &mut cred,
        crate::providers::outlook::GRAPH_SCOPE,
        email,
    )
    .await
    .map_err(|e| AppError::bad_gateway(format!("Graph 获取邮件失败: {e}")))?;

    // 列表页**不取正文**：Python 版的 `graph_list_messages` 也是这个 `$select`。
    // 正文动辄几十 KB，20 封就 1MB+，而列表只显示预览。
    let msgs = crate::providers::graph::list_messages_opts(&http, &token, folder, top, None, false)
        .await
        .map_err(|e| AppError::bad_gateway(format!("Graph 获取邮件失败: {e}")))?;

    // 落库（管理端看过的信也进 mail_records，便于事后检索）
    for m in &msgs {
        let mail = crate::providers::graph::normalize_message(m);
        if mail.message_id.is_empty() {
            continue;
        }
        let received = crate::util::normalize_utc(&mail.received_at).map(|d| d.naive_utc());
        let _ = crate::services::mail::save_mail_record(
            pool,
            email,
            "",
            &mail.message_id,
            "outlook",
            folder,
            &mail.sender,
            &mail.subject,
            &mail.body_preview,
            &mail.body,
            "",
            "admin",
            received,
        )
        .await;
    }

    Ok(Value::Array(msgs))
}

/// 取单封邮件全文。
pub async fn get_message(pool: &SqlitePool, email: &str, message_id: &str) -> AppResult<Value> {
    let (mut cred, http) = cred_for(pool, email).await?;
    let token = crate::providers::outlook::access_token(
        &http,
        &mut cred,
        crate::providers::outlook::GRAPH_SCOPE,
        email,
    )
    .await
    .map_err(|e| AppError::bad_gateway(format!("Graph 获取邮件失败: {e}")))?;

    // 单封：打 /me/messages/{id}，**明确 $select 正文相关字段**。
    // 不指定 $select 会返回 Graph 的全部字段（含大量 @odata 元数据、
    // internetMessageHeaders 等），一个响应能到几百 KB。
    let url = format!(
        "{}/me/messages/{}?$select=id,subject,body,bodyPreview,receivedDateTime,from,toRecipients,isRead",
        crate::providers::outlook::GRAPH_BASE,
        urlencoding(message_id)
    );
    let resp = http
        .get(&url)
        .bearer_auth(&token)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| AppError::bad_gateway(format!("Graph 获取邮件失败: {e}")))?;
    if !resp.status().is_success() {
        let st = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(AppError::bad_gateway(format!(
            "Graph 获取邮件失败: HTTP {st} {}",
            crate::util::truncate_chars(&text, 200)
        )));
    }
    resp.json::<Value>()
        .await
        .map_err(|e| AppError::bad_gateway(format!("Graph 响应非 JSON: {e}")))
}

/// URL 路径段编码（message_id 里可能有 `/ + =` 等需要转义的字符）。
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_extracts_from_body() {
        let mail = json!({"body": "您的验证码是 483920"});
        assert_eq!(extract_rule(&mail), "483920");
    }

    #[test]
    fn rule_prefers_explicit_field() {
        let mail = json!({"code": "111222", "body": "code is 999888"});
        assert_eq!(extract_rule(&mail), "111222");
    }

    #[test]
    fn rule_empty_when_nothing() {
        assert_eq!(extract_rule(&json!({"body": "hello world"})), "");
    }

    #[test]
    fn urlencode_path_segment() {
        // Graph message id 里常见 `AAMkAD...=` 这类字符
        assert_eq!(urlencoding("abc123"), "abc123");
        assert_eq!(urlencoding("a=b/c+d"), "a%3Db%2Fc%2Bd");
    }

    #[tokio::test]
    async fn ai_disabled_without_env_falls_back_to_rule() {
        // 没配环境变量时必须静默回退，不能报错
        let http = reqwest::Client::new();
        let mail = json!({"body": "code is 556677"});
        let r = extract_ai(&http, &mail, true).await;
        assert_eq!(r.verification_code, "556677");
        assert_eq!(r.reason, "rule");
        assert_eq!(r.confidence, 1.0);
    }

    #[tokio::test]
    async fn ai_use_ai_false_skips_model() {
        let http = reqwest::Client::new();
        let mail = json!({"body": "code is 123456"});
        let r = extract_ai(&http, &mail, false).await;
        assert_eq!(r.verification_code, "123456");
    }
}

// ────────────────────────────────────────────────────────────────
//  路由 handler
// ────────────────────────────────────────────────────────────────

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;

use crate::auth::{AppState, Identity};
use crate::error::{admin_ok};

fn ensure_admin(identity: &Identity) -> AppResult<()> {
    if identity.is_admin {
        Ok(())
    } else {
        Err(AppError::unauthorized("需要管理员口令（X-Admin-Password 或后台登录）"))
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct MessagesQuery {
    pub email: String,
    #[serde(default = "default_folder")]
    pub folder: String,
    #[serde(default = "default_top")]
    pub top: u32,
}

fn default_folder() -> String {
    "inbox".to_string()
}
fn default_top() -> u32 {
    20
}

/// `GET /api/admin/mailbox/messages`
pub async fn admin_messages(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Query(q): Query<MessagesQuery>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let msgs = list_messages(&st.pool, q.email.trim(), &q.folder, q.top.clamp(1, 100)).await?;
    Ok(admin_ok(msgs))
}

#[derive(Debug, serde::Deserialize)]
pub struct MessageQuery {
    pub email: String,
    pub message_id: String,
}

/// `GET /api/admin/mailbox/message`
pub async fn admin_message(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Query(q): Query<MessageQuery>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let msg = get_message(&st.pool, q.email.trim(), &q.message_id).await?;
    Ok(admin_ok(msg))
}

#[derive(Debug, serde::Deserialize)]
pub struct AiCodeBody {
    pub email: String,
    #[serde(default)]
    pub message: Value,
    #[serde(default = "default_true")]
    pub use_ai: bool,
}

fn default_true() -> bool {
    true
}

/// `POST /api/admin/mailbox/ai-code`
pub async fn admin_ai_code(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<AiCodeBody>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    // 先校验这个邮箱确实在池里（对齐 Python 的 `_outlook_cred` 前置校验），
    // 否则任意 email 都能触发一次 LLM 调用，等于白送 token 消耗。
    let _ = cred_for(&st.pool, body.email.trim()).await?;
    let r = extract_ai(&st.http, &body.message, body.use_ai).await;
    Ok(admin_ok(serde_json::to_value(r)?))
}