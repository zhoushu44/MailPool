//! 私有邮箱：客户上传自己的 Outlook 凭据，平台统一代收件。
//!
//! ## 定位（对齐 MailNest「私有邮箱托管」）
//!
//! > 上传自己的 Outlook 邮箱（Refresh Token），由平台统一收件、查询邮件和调用 API。
//!
//! 与另外两条产品线的区别：
//!
//! | | 临时邮箱 | 独占邮箱 | 私有邮箱 |
//! |---|---|---|---|
//! | 号的来源 | 平台池 | 平台池 | **客户自己的号** |
//! | 计费 | 收码扣费 | 买断 | 不占用平台库存，免费 |
//! | 用途 | 平台项目注册 | 长期持有 | 客户自己业务的收件 |
//!
//! ## 上传格式（对齐 MailNest 的交付格式）
//!
//! 每行一条，`----` 分隔，兼容三种：
//!
//! ```text
//! 邮箱----密码----ClientID----RefreshToken   （完整 4 段，MailNest 交付格式）
//! 邮箱----ClientID----RefreshToken           （3 段，无密码）
//! {"email":"...","client_id":"...","refresh_token":"..."}   （JSON 行）
//! ```
//!
//! 密码字段不使用（平台只用 OAuth 凭据），但保留在解析里防止格式误判。
//!
//! ## 验证（上传时逐条）
//!
//! 刷新 token → Graph `/me` → 比对返回的 `userPrincipalName` 与填写的邮箱：
//! - token 失效 → status=invalid，带原因（不入池，但记录供用户修正）
//! - 邮箱不匹配 → 用 /me 返回的真实地址覆盖（客户经常填错大小写/别名）
//!
//! ## 安全说明
//!
//! `account_json` 里是 refresh_token，与平台号池（email_accounts）的存储方式一致
//! （明文存 SQLite）。库文件本身在 data/ 下，靠文件系统权限保护 —— 和现状
//! 同一威胁模型，不额外引入加密复杂度。

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};
use crate::util::now_naive;

/// 一条私有邮箱记录。
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PrivateEmail {
    pub id: i64,
    pub user_id: i64,
    pub email: String,
    pub provider: String,
    /// {client_id, refresh_token}
    pub account_json: String,
    /// pending / active / invalid
    pub status: String,
    pub status_reason: String,
    pub created_at: NaiveDateTime,
    pub last_check_at: Option<NaiveDateTime>,
}

/// 解析后的一条上传项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadItem {
    pub email: String,
    pub client_id: String,
    pub refresh_token: String,
}

/// 解析上传文本。
///
/// 见模块文档的三种格式。空行 / 注释行（# 开头）跳过。
pub fn parse_upload_text(text: &str) -> Vec<(UploadItem, String)> {
    let mut out = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line_no = idx + 1;

        // JSON 行
        if line.starts_with('{') {
            match serde_json::from_str::<UploadItem>(line) {
                Ok(it) if !it.email.is_empty() && !it.client_id.is_empty() && !it.refresh_token.is_empty() => {
                    out.push((it, String::new()));
                }
                Ok(_) => out.push((bad(), format!("第 {line_no} 行：JSON 缺字段"))),
                Err(e) => out.push((bad(), format!("第 {line_no} 行：JSON 解析失败 {e}"))),
            }
            continue;
        }

        // ---- 分隔格式
        let parts: Vec<&str> = line.split("----").map(|p| p.trim()).collect();
        let it = match parts.len() {
            4 => UploadItem {
                // 4 段 = 邮箱----密码----ClientID----RefreshToken（密码弃用）
                email: parts[0].to_string(),
                client_id: parts[2].to_string(),
                refresh_token: parts[3].to_string(),
            },
            3 => UploadItem {
                email: parts[0].to_string(),
                client_id: parts[1].to_string(),
                refresh_token: parts[2].to_string(),
            },
            _ => {
                out.push((bad(), format!("第 {line_no} 行：需要 3 或 4 段（---- 分隔），实际 {} 段", parts.len())));
                continue;
            }
        };

        if it.email.contains('@') && !it.client_id.is_empty() && !it.refresh_token.is_empty() {
            out.push((it, String::new()));
        } else {
            out.push((bad(), format!("第 {line_no} 行：邮箱/ClientID/RefreshToken 不能为空")));
        }
    }
    out
}

fn bad() -> UploadItem {
    UploadItem {
        email: String::new(),
        client_id: String::new(),
        refresh_token: String::new(),
    }
}

/// 建表（幂等）。
pub async fn ensure_schema(pool: &SqlitePool) -> AppResult<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS private_emails (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL,
            email VARCHAR(254) NOT NULL,
            provider VARCHAR(32) NOT NULL DEFAULT 'outlook',
            account_json TEXT NOT NULL DEFAULT '{}',
            status VARCHAR(16) NOT NULL DEFAULT 'pending',
            status_reason VARCHAR(255) NOT NULL DEFAULT '',
            created_at DATETIME NOT NULL,
            last_check_at DATETIME,
            CONSTRAINT uq_private_email_user UNIQUE (email, user_id)
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS ix_private_user ON private_emails (user_id, status)")
        .execute(pool)
        .await?;
    Ok(())
}

/// 验证一条凭据：刷新 token → Graph /me → 返回真实邮箱。
///
/// 返回 `(真实邮箱, Ok)`；Err 带人话原因。
pub async fn verify_credentials(
    http: &reqwest::Client,
    client_id: &str,
    refresh_token: &str,
) -> Result<String, String> {
    let mut cred = crate::providers::outlook::Credentials {
        client_id: client_id.to_string(),
        refresh_token: refresh_token.to_string(),
        email: String::new(),
        proxy: String::new(),
        extra: serde_json::Map::new(),
    };

    // 刷新 token —— 失败即凭据无效，一步到位
    let token = crate::providers::outlook::access_token(
        http,
        &mut cred,
        crate::providers::outlook::GRAPH_SCOPE,
        "verify@private",
    )
    .await
    .map_err(|e| format!("token 刷新失败: {}", crate::util::truncate_chars(&e.to_string(), 120)))?;

    // 从 JWT payload 里取真实邮箱。
    //
    // 不用 Graph /me：这个 client_id 的授权只有收信权限（没有 User.Read），
    // 实测 /me 返回 401 —— 但读邮件（/me/messages）一直是好的，
    // 说明号本身能用。JWT 是微软自己签发的，claims 里的
    // preferred_username 就是账号地址，解出来即可，无需网络请求。
    jwt_email(&token).ok_or_else(|| "token 里没有邮箱信息（claims 缺 preferred_username）".to_string())
}

/// 解 access_token（JWT）的 payload，取账号地址。
///
/// 不校验签名 —— token 是刚从微软 token 端点拿到的（TLS 保证来源），
/// 这里只是读自己的 claims，不是鉴权场景。
fn jwt_email(token: &str) -> Option<String> {
    let payload_b64 = token.split('.').nth(1)?;
    // JWT 用 URL-safe base64，无 padding
    let bytes = base64url_decode(payload_b64)?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    for key in ["preferred_username", "upn", "email", "unique_name"] {
        if let Some(v) = claims.get(key).and_then(|v| v.as_str()) {
            let v = v.trim().to_ascii_lowercase();
            if v.contains('@') {
                return Some(v);
            }
        }
    }
    None
}

/// URL-safe base64 解码（无 padding）。
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut buf = String::with_capacity(input.len() + 4);
    for c in input.chars() {
        match c {
            '-' => buf.push('+'),
            '_' => buf.push('/'),
            _ => buf.push(c),
        }
    }
    while buf.len() % 4 != 0 {
        buf.push('=');
    }
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(buf.as_bytes()).ok()
}

/// 上传一条（验证后入库）。已存在则更新凭据并重置状态。
///
/// 返回入库结果摘要。
pub async fn upsert(
    pool: &SqlitePool,
    http: &reqwest::Client,
    user_id: i64,
    item: &UploadItem,
) -> (String, String, bool) {
    // (邮箱, 原因, 是否成功)
    let (final_email, reason, ok) = match verify_credentials(http, &item.client_id, &item.refresh_token).await {
        Ok(real) => {
            let declared = item.email.trim().to_ascii_lowercase();
            // 客户填错地址时以 /me 为准；一致时用客户的（保大小写习惯）
            if real != declared && !real.is_empty() {
                (real.clone(), format!("已按实际地址 {} 修正", real), true)
            } else {
                (declared, String::new(), true)
            }
        }
        Err(e) => (item.email.trim().to_ascii_lowercase(), e, false),
    };

    if final_email.is_empty() {
        return (String::new(), "邮箱为空".to_string(), false);
    }

    let status = if ok { "active" } else { "invalid" };
    let account_json = serde_json::json!({
        "client_id": item.client_id.trim(),
        "refresh_token": item.refresh_token.trim(),
    })
    .to_string();

    // 同一 (email, user) 已有 → 更新；否则插入
    let updated = sqlx::query(
        "UPDATE private_emails \
         SET account_json = ?, status = ?, status_reason = ?, last_check_at = ? \
         WHERE user_id = ? AND lower(email) = ?",
    )
    .bind(&account_json)
    .bind(status)
    .bind(&reason)
    .bind(now_naive())
    .bind(user_id)
    .bind(&final_email)
    .execute(pool)
    .await;

    match updated {
        Ok(r) if r.rows_affected() > 0 => {}
        _ => {
            let _ = sqlx::query(
                "INSERT OR IGNORE INTO private_emails \
                 (user_id, email, provider, account_json, status, status_reason, created_at, last_check_at) \
                 VALUES (?, ?, 'outlook', ?, ?, ?, ?, ?)",
            )
            .bind(user_id)
            .bind(&final_email)
            .bind(&account_json)
            .bind(status)
            .bind(&reason)
            .bind(now_naive())
            .bind(now_naive())
            .execute(pool)
            .await;
        }
    }

    (final_email, reason, ok)
}

/// 我的私有邮箱列表。
pub async fn list_mine(pool: &SqlitePool, user_id: i64) -> AppResult<Vec<PrivateEmail>> {
    Ok(sqlx::query_as::<_, PrivateEmail>(
        "SELECT * FROM private_emails WHERE user_id = ? ORDER BY status = 'active' DESC, id DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

/// 删除一条（只能删自己的）。
pub async fn remove(pool: &SqlitePool, user_id: i64, email: &str) -> AppResult<bool> {
    let r = sqlx::query("DELETE FROM private_emails WHERE user_id = ? AND lower(email) = lower(?)")
        .bind(user_id)
        .bind(email.trim())
        .execute(pool)
        .await?;
    Ok(r.rows_affected() > 0)
}

/// 重新验证一条。
pub async fn reverify(
    pool: &SqlitePool,
    http: &reqwest::Client,
    user_id: i64,
    email: &str,
) -> AppResult<(bool, String)> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT account_json FROM private_emails WHERE user_id = ? AND lower(email) = lower(?)",
    )
    .bind(user_id)
    .bind(email.trim())
    .fetch_optional(pool)
    .await?;

    let Some((json,)) = row else {
        return Err(AppError::not_found("记录不存在"));
    };
    let cred: crate::providers::outlook::Credentials =
        serde_json::from_str(&json).map_err(|e| AppError::bad_request(format!("凭据损坏: {e}")))?;

    let (status, reason) = match verify_credentials(http, &cred.client_id, &cred.refresh_token).await {
        Ok(real) => {
            let real_low = real.to_ascii_lowercase();
            let declared = email.trim().to_ascii_lowercase();
            if real_low != declared {
                (String::new(), format!("token 实际属于 {real_low}，与记录不一致"))
            } else {
                ("active".to_string(), String::new())
            }
        }
        Err(e) => ("invalid".to_string(), e),
    };

    // status 为空 = 地址不一致，标 invalid 并说明
    let status = if status.is_empty() { "invalid".to_string() } else { status };
    sqlx::query(
        "UPDATE private_emails SET status = ?, status_reason = ?, last_check_at = ? \
         WHERE user_id = ? AND lower(email) = lower(?)",
    )
    .bind(&status)
    .bind(&reason)
    .bind(now_naive())
    .bind(user_id)
    .bind(email.trim())
    .execute(pool)
    .await?;

    Ok((status == "active", reason))
}

/// 是否拥有该邮箱（active）—— 给收件页归属校验用。
pub async fn owns(pool: &SqlitePool, user_id: i64, email: &str) -> AppResult<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM private_emails \
         WHERE user_id = ? AND lower(email) = lower(?) AND status = 'active'",
    )
    .bind(user_id)
    .bind(email)
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

/// 拿某邮箱的凭据（收件用）。
pub async fn credentials_for(
    pool: &SqlitePool,
    email: &str,
) -> AppResult<Option<crate::providers::outlook::Credentials>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT account_json FROM private_emails WHERE lower(email) = lower(?) AND status = 'active' LIMIT 1",
    )
    .bind(email.trim())
    .fetch_optional(pool)
    .await?;
    match row {
        Some((json,)) => Ok(Some(serde_json::from_str(&json).map_err(|e| {
            AppError::internal(format!("私有邮箱凭据损坏: {e}"))
        })?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_email_extracts_preferred_username() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::STANDARD
            .encode(br#"{"typ":"JWT","preferred_username":"A@X.test"}"#);
        let jwt = format!("eyJh.{payload}.sig");
        assert_eq!(jwt_email(&jwt).unwrap(), "a@x.test", "应取出并转小写");
    }

    #[test]
    fn jwt_email_tries_fallback_keys() {
        use base64::Engine;
        let payload =
            base64::engine::general_purpose::STANDARD.encode(br#"{"upn":"b@y.test"}"#);
        let jwt = format!("h.{payload}.s");
        assert_eq!(jwt_email(&jwt).unwrap(), "b@y.test");
    }

    #[test]
    fn jwt_email_rejects_token_without_email() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::STANDARD.encode(br#"{"aud":"x"}"#);
        let jwt = format!("h.{payload}.s");
        assert!(jwt_email(&jwt).is_none());
    }

    #[test]
    fn base64url_handles_url_safe_chars() {
        use base64::Engine;
        let orig = br#"{"email":"c@z.test"}"#;
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(orig);
        let dec = base64url_decode(&enc).unwrap();
        assert_eq!(dec, orig);
    }

    #[test]
    fn parses_four_field_lines() {
        let t = "a@x.test----pw123----cid-1----rt-1\nb@x.test----pw----cid-2----rt-2";
        let items = parse_upload_text(t);
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|(it, e)| e.is_empty()));
        assert_eq!(items[0].0.email, "a@x.test");
        assert_eq!(items[0].0.client_id, "cid-1"); // 第 3 段，不是密码
        assert_eq!(items[0].0.refresh_token, "rt-1");
    }

    #[test]
    fn parses_three_field_lines() {
        let t = "a@x.test----cid-1----rt-1";
        let items = parse_upload_text(t);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0.client_id, "cid-1");
    }

    #[test]
    fn parses_json_lines() {
        let t = r#"{"email":"a@x.test","client_id":"c1","refresh_token":"r1"}"#;
        let items = parse_upload_text(t);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0.email, "a@x.test");
    }

    #[test]
    fn skips_blank_and_comment() {
        let t = "\n# 注释\na@x.test----c----r\n";
        let items = parse_upload_text(t);
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn reports_bad_lines_with_numbers() {
        let t = "a@x.test----only-two\nok@x.test----c----r";
        let items = parse_upload_text(t);
        assert_eq!(items.len(), 2);
        assert!(items[0].1.contains("第 1 行"));
        assert!(items[0].0.email.is_empty());
        assert!(items[1].1.is_empty());
    }

    #[test]
    fn rejects_missing_fields() {
        let t = "no-at-sign----c----r";
        let items = parse_upload_text(t);
        assert!(!items[0].1.is_empty(), "无 @ 的邮箱要报错");
    }

    use std::str::FromStr;

    async fn setup() -> SqlitePool {
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
        crate::db::init_db(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn remove_only_own_rows() {
        let pool = setup().await;
        sqlx::query(
            "INSERT INTO private_emails (user_id, email, account_json, status, created_at) \
             VALUES (1, 'a@x.test', '{}', 'active', ?)",
        )
        .bind(now_naive())
        .execute(&pool)
        .await
        .unwrap();

        // 用户 2 删不掉用户 1 的
        assert!(!remove(&pool, 2, "a@x.test").await.unwrap());
        assert!(remove(&pool, 1, "A@X.TEST").await.unwrap()); // 大小写不敏感
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM private_emails")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn owns_requires_active() {
        let pool = setup().await;
        for (st, owns_expect) in [("active", true), ("invalid", false)] {
            let email = format!("{}@x.test", st);
            sqlx::query(
                "INSERT INTO private_emails (user_id, email, account_json, status, created_at) \
                 VALUES (1, ?, '{}', ?, ?)",
            )
            .bind(&email)
            .bind(st)
            .bind(now_naive())
            .execute(&pool)
            .await
            .unwrap();
            assert_eq!(owns(&pool, 1, &email).await.unwrap(), owns_expect, "{st}");
        }
    }
}
