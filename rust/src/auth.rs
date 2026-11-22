//! 会话与鉴权。
//!
//! ## 会话 cookie 与 Python 版**完全兼容**
//!
//! Python 用 `itsdangerous.URLSafeTimedSerializer(secret, salt="mailpool-session")`，
//! 其 token 形如 `base64url(json).base64url(timestamp).base64url(hmac_sha1)`，
//! 签名算法是 **django-concat + SHA1**（itsdangerous 2.x 默认，不是 SHA256）：
//!
//! ```text
//! key = SHA1(salt || "signer" || secret)
//! sig = HMAC-SHA1(key, payload_b64 || "." || ts_b64)
//! ```
//!
//! 这里逐字节复刻，因此 **Rust 版能直接读 Python 版签发的 cookie，反之亦然** ——
//! 迁移时用户不需要重新登录，灰度期间两个进程可以共用同一个 cookie。
//!
//! ## 身份模型（对齐 `app/api/auth.py`）
//!
//! - **管理员**：会话 `admin_ok=true`，或请求头 `X-Admin-Password` 正确；
//!   未配置口令时仅回环地址放行。
//! - **客户**：会话 `user_id`，或 Bearer API Key（`api_keys` 表绑 user_id）。
//! - **匿名 key**：`settings.api_keys` 里的老 key，可调用但计费按 0。

use std::collections::HashMap;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::HeaderMap;
use axum_extra::extract::CookieJar;

use crate::config::Settings;
use crate::error::{AppError, AppResult};
use crate::models::ApiKey;
use crate::util::constant_time_eq;

pub const COOKIE_NAME: &str = "mailpool_session";
/// 会话有效期（对齐 Python `_MAX_AGE = timedelta(days=7)`）
pub const MAX_AGE_SECS: i64 = 7 * 24 * 3600;
const SALT: &str = "mailpool-session";

// ────────────────────────────────────────────────────────────────
//  签名 / 验签
// ────────────────────────────────────────────────────────────────

fn b64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .ok()
}

/// django-concat 派生密钥：`SHA1(salt || "signer" || secret)`。
fn derive_key(secret: &str) -> Vec<u8> {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(SALT.as_bytes());
    h.update(b"signer");
    h.update(secret.as_bytes());
    h.finalize().to_vec()
}

fn sign(value: &[u8], key: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("任何长度都能做 HMAC key");
    mac.update(value);
    mac.finalize().into_bytes().to_vec()
}

/// 会话数据（`HashMap` 因为键不稳定：`admin_ok` / `user_id` / `username` 可任意组合）。
pub type SessionData = HashMap<String, serde_json::Value>;

/// 签发会话 token（用指定 secret）。
///
/// 拆出 `_with_secret` 版本是为了能对着 Python 产出的 token 做跨语言回归测试
/// （全局 `Settings` 是 OnceLock，测试里没法可靠地改）。
pub fn dumps_with_secret(data: &SessionData, secret: &str) -> String {
    let key = derive_key(secret);

    // 必须用紧凑 JSON（无空格），否则 base64 与 Python 不一致
    let json = serde_json::to_vec(data).unwrap_or_else(|_| b"{}".to_vec());
    let payload = b64_encode(&json);
    // itsdangerous 的时间戳是 **4 字节大端 uint32**，不是十进制字符串。
    // （URLSafeTimedSerializer → TimestampSigner.get_timestamp）
    let ts = (crate::util::utc_now().timestamp() as u32).to_be_bytes();
    let ts_b64 = b64_encode(&ts);

    let value = format!("{payload}.{ts_b64}");
    let sig = b64_encode(&sign(value.as_bytes(), &key));
    format!("{value}.{sig}")
}

/// 验证并解析会话 token（用指定 secret）。失败返回 `None`。
pub fn loads_with_secret(token: &str, secret: &str) -> Option<SessionData> {
    let key = derive_key(secret);

    let mut it = token.split('.');
    let payload_b64 = it.next()?;
    let ts_b64 = it.next()?;
    let sig_b64 = it.next()?;
    if it.next().is_some() {
        return None;
    }

    // 1) 校验签名（先验签再解析，避免对不可信数据做 JSON 解析）
    let value = format!("{payload_b64}.{ts_b64}");
    let expected = b64_encode(&sign(value.as_bytes(), &key));
    if !constant_time_eq(&expected, sig_b64) {
        return None;
    }

    // 2) 校验时效（itsdangerous 的 TimestampSigner 语义：4 字节大端 uint32）
    let ts_raw = b64_decode(ts_b64)?;
    if ts_raw.len() != 4 {
        return None;
    }
    let ts = u32::from_be_bytes([ts_raw[0], ts_raw[1], ts_raw[2], ts_raw[3]]) as i64;
    let age = crate::util::utc_now().timestamp() - ts;
    if age < 0 || age > MAX_AGE_SECS {
        return None;
    }

    // 3) 解析载荷
    let raw = b64_decode(payload_b64)?;
    serde_json::from_slice::<SessionData>(&raw).ok()
}

/// 签发会话 token（用全局配置的 secret）。
pub fn dumps(data: &SessionData) -> String {
    dumps_with_secret(data, &Settings::get().session_secret)
}

/// 验证并解析会话 token（用全局配置的 secret）。
pub fn loads(token: &str) -> Option<SessionData> {
    loads_with_secret(token, &Settings::get().session_secret)
}

// ────────────────────────────────────────────────────────────────
//  身份
// ────────────────────────────────────────────────────────────────

/// 当前请求的身份。
#[derive(Debug, Clone, Default)]
pub struct Identity {
    /// 会话数据（可能为空）
    pub session: SessionData,
    /// 是否管理员（会话 admin_ok 或口令头正确）
    pub is_admin: bool,
    /// 客户 user_id（会话登录或 API Key 绑定）
    pub user_id: Option<i64>,
    /// 是否带了一个**合法**的 Bearer Key（含匿名老 key）
    pub has_valid_key: bool,
    /// 是否匿名老 key（无归属用户 → 不计费）
    pub is_anonymous_key: bool,
    /// 客户端地址
    pub client_ip: String,
}

impl Identity {
    pub fn is_customer(&self) -> bool {
        self.user_id.is_some()
    }

    /// 会话里的用户名（页面展示用）。
    pub fn username(&self) -> String {
        self.session
            .get("username")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }
}

/// 从请求里抽 Bearer token。
pub fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = raw.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// 校验 Bearer Key。返回 `(valid, user_id, is_anonymous)`。
///
/// 三种 key 都认（对齐 Python `_verify_api_key`）：
/// - `settings.api_keys` 里的老 key（匿名，不计费）
/// - `api_keys` 表里绑了 user_id 的 key（计费）
/// - 完全没有配置任何 key → 本机自用，放行（`valid=true, anonymous=true`）
pub async fn verify_api_key(
    pool: &sqlx::SqlitePool,
    headers: &HeaderMap,
) -> AppResult<(bool, Option<i64>, bool)> {
    let Some(token) = bearer_token(headers) else {
        return Ok((false, None, false));
    };

    // 1) 老式匿名 key
    let legacy = crate::services::settings::get_api_keys(pool).await?;
    if legacy.iter().any(|k| constant_time_eq(k, &token)) {
        return Ok((true, None, true));
    }

    // 2) 绑定用户的 key
    let row = sqlx::query_as::<_, ApiKey>("SELECT * FROM api_keys WHERE key = ?")
        .bind(&token)
        .fetch_optional(pool)
        .await?;
    if let Some(k) = row {
        if k.enabled != 0 {
            // 用户必须存在且启用
            if k.user_id > 0 {
                if let Some(u) = crate::services::billing::get_user(pool, k.user_id).await? {
                    if u.is_active() {
                        return Ok((true, Some(u.id), false));
                    }
                }
                return Ok((false, None, false));
            }
            return Ok((true, None, true));
        }
        return Ok((false, None, false));
    }

    // 3) 一个 key 都没配 → 本机自用模式，放行（匿名）
    if legacy.is_empty() {
        let any: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys")
            .fetch_one(pool)
            .await?;
        if any == 0 {
            return Ok((true, None, true));
        }
    }
    Ok((false, None, false))
}

/// 管理员是否放行（不抛异常，便于页面路由复用做重定向）。
pub fn check_admin_access(
    session: &SessionData,
    headers: &HeaderMap,
    client_ip: &str,
) -> bool {
    if session
        .get("admin_ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return true;
    }

    let settings = Settings::get();
    if settings.has_admin_password() {
        if let Some(pw) = headers
            .get("x-admin-password")
            .and_then(|v| v.to_str().ok())
        {
            if !pw.is_empty() && constant_time_eq(pw, &settings.admin_password) {
                return true;
            }
        }
        return false;
    }

    // 未配置口令：仅回环地址免密
    crate::util::is_loopback(client_ip)
}

/// 校验管理员口令（登录表单）。
pub fn verify_admin_password(input: &str) -> bool {
    let settings = Settings::get();
    settings.has_admin_password() && constant_time_eq(input, &settings.admin_password)
}

// ────────────────────────────────────────────────────────────────
//  密码哈希（PBKDF2-SHA256，200k 轮，与 Python 版字节兼容）
// ────────────────────────────────────────────────────────────────

/// 格式：`pbkdf2_sha256$200000$<salt_hex>$<digest_hex>`
pub fn hash_password(password: &str) -> String {
    let salt = {
        use rand::RngCore;
        let mut b = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut b);
        b
    };
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), &salt, 200_000, &mut out);
    format!(
        "pbkdf2_sha256$200000${}${}",
        hex::encode(salt),
        hex::encode(out)
    )
}

/// 校验密码。任何格式错误都返回 false（对齐 Python 的 try/except return False）。
pub fn verify_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 4 {
        return false;
    }
    let (algo, rounds, salt_hex, digest_hex) = (parts[0], parts[1], parts[2], parts[3]);
    if algo != "pbkdf2_sha256" {
        return false;
    }
    let Ok(rounds) = rounds.parse::<u32>() else {
        return false;
    };
    let Ok(salt) = hex::decode(salt_hex) else {
        return false;
    };
    // 上限保护：拒绝天文数字轮数（DoS）
    if rounds == 0 || rounds > 10_000_000 {
        return false;
    }
    let expected_len = digest_hex.len() / 2;
    let mut out = vec![0u8; expected_len.max(1)];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), &salt, rounds, &mut out);
    constant_time_eq(&hex::encode(out), digest_hex)
}

// ────────────────────────────────────────────────────────────────
//  Axum 提取器
// ────────────────────────────────────────────────────────────────

/// 应用状态（见 `main.rs`）。
pub struct AppState {
    pub pool: sqlx::SqlitePool,
    pub http: reqwest::Client,
    /// 进程内统计（对齐 Python `_OPERATION_STATS`）
    pub stats: std::sync::Arc<Stats>,
    /// 模板引擎（页面层用；模板已预编译，可跨请求共享）
    pub templates: std::sync::Arc<crate::web::templates::Templates>,
}

/// 进程内操作计数。
#[derive(Debug, Default)]
pub struct Stats {
    pub claims: std::sync::atomic::AtomicU64,
    pub codes: std::sync::atomic::AtomicU64,
    pub imports: std::sync::atomic::AtomicU64,
    pub revoked: std::sync::atomic::AtomicU64,
}

impl Stats {
    pub fn snapshot(&self) -> serde_json::Value {
        use std::sync::atomic::Ordering::Relaxed;
        serde_json::json!({
            "claims": self.claims.load(Relaxed),
            "codes": self.codes.load(Relaxed),
            "imports": self.imports.load(Relaxed),
            "revoked": self.revoked.load(Relaxed),
        })
    }
}

/// 自动注入 `AppState` 的 `FromRequestParts`，供 handler 直接取。
impl FromRequestParts<std::sync::Arc<AppState>> for Identity {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &std::sync::Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let jar = CookieJar::from_headers(&parts.headers);
        let session = jar
            .get(COOKIE_NAME)
            .and_then(|c| loads(c.value()))
            .unwrap_or_default();

        let client_ip = parts
            .extensions
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|ci| ci.0.ip().to_string())
            .unwrap_or_default();

        let is_admin = check_admin_access(&session, &parts.headers, &client_ip);

        // 会话里的客户身份
        let session_uid = session
            .get("user_id")
            .and_then(|v| v.as_i64())
            .filter(|id| *id > 0);

        let (has_key, key_uid, anon_key) = verify_api_key(&state.pool, &parts.headers).await?;

        // API Key 优先于会话（对齐 Python `current_user`：先 key 再会话）
        let user_id = if has_key {
            key_uid
        } else {
            session_uid
        };

        Ok(Identity {
            session,
            is_admin,
            user_id,
            has_valid_key: has_key,
            is_anonymous_key: anon_key,
            client_ip,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip() {
        let h = hash_password("alicepass");
        assert!(verify_password("alicepass", &h));
        assert!(!verify_password("wrong", &h));
        assert!(h.starts_with("pbkdf2_sha256$200000$"));
    }

    #[test]
    fn password_format_matches_python() {
        // Python: f"pbkdf2_sha256${rounds}${salt.hex()}${digest.hex()}"
        let h = hash_password("x");
        let parts: Vec<&str> = h.split('$').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "pbkdf2_sha256");
        assert_eq!(parts[1], "200000");
        assert_eq!(parts[2].len(), 32); // 16 字节 salt = 32 hex
        assert_eq!(parts[3].len(), 64); // 32 字节 digest = 64 hex
    }

    #[test]
    fn python_password_hash_verifies_in_rust() {
        // 由 Python hashlib.pbkdf2_hmac("sha256", b"alicepass", salt, 200_000) 生成。
        // 能验过 = 迁移后老用户的密码继续可用，不需要重置。
        const PY_HASH: &str = "pbkdf2_sha256$200000$aabbccddeeff00112233445566778899$6ee80d8f4669be34b87c9af6a9c8cc540847da2dc7c3833376860a51f50bfdcd";
        assert!(verify_password("alicepass", PY_HASH), "Python 生成的哈希必须能在 Rust 侧验证");
        assert!(!verify_password("wrongpass", PY_HASH));
        assert!(!verify_password("", PY_HASH));
    }

    #[test]
    fn rust_password_hash_verifies_in_rust() {
        // 双向：Rust 生成的哈希也要符合 Python 的格式约定
        let h = hash_password("hello-world-42");
        assert!(verify_password("hello-world-42", &h));
    }

    #[test]
    fn verify_rejects_garbage() {
        assert!(!verify_password("x", ""));
        assert!(!verify_password("x", "not$a$hash"));
        assert!(!verify_password("x", "bcrypt$10$aa$bb"));
        assert!(!verify_password("x", "pbkdf2_sha256$abc$aa$bb"));
        assert!(!verify_password("x", "pbkdf2_sha256$0$aa$bb"));
    }

    #[test]
    fn session_roundtrip() {
        let mut d = SessionData::new();
        d.insert("user_id".into(), serde_json::json!(7));
        d.insert("username".into(), serde_json::json!("alice"));
        let tok = dumps(&d);
        let back = loads(&tok).expect("应能解回");
        assert_eq!(back.get("user_id").unwrap().as_i64(), Some(7));
        assert_eq!(back.get("username").unwrap().as_str(), Some("alice"));
    }

    #[test]
    fn session_tampered_rejected() {
        let mut d = SessionData::new();
        d.insert("admin_ok".into(), serde_json::json!(true));
        let tok = dumps(&d);
        // 改载荷 → 签名不再匹配
        let mut parts: Vec<&str> = tok.split('.').collect();
        parts[0] = "eyJhZG1pbl9vayI6ZmFsc2V9";
        let bad = parts.join(".");
        assert!(loads(&bad).is_none(), "篡改载荷后必须验签失败");
    }

    #[test]
    fn session_garbage_rejected() {
        assert!(loads("").is_none());
        assert!(loads("a.b").is_none());
        assert!(loads("a.b.c.d").is_none());
        assert!(loads("....").is_none());
    }

    // ── 跨语言兼容（关键回归，防止迁移时被迫让用户重新登录）──
    //
    // 这些 token 由 **Python itsdangerous** 用 secret
    // "PYTHON1234567890abcdefghijklmnopqrstuvwxyz" 真实签发。
    // 能验证通过 = Rust 版认得 Python 版留下的会话 cookie。
    #[test]
    fn python_itsdangerous_tokens_verify_in_rust() {
        const SECRET: &str = "PYTHON1234567890abcdefghijklmnopqrstuvwxyz";
        let cases: &[(&str, &str, &str)] = &[
            (
                "eyJ1c2VyX2lkIjo3LCJ1c2VybmFtZSI6ImFsaWNlIn0.awJLtw.DlgYJ6_3HGT_UXqWKPWitaqRf5M",
                "user_id",
                "7",
            ),
            (
                "eyJhZG1pbl9vayI6dHJ1ZX0.awJLtw.G6vNzmCv4HSvmudM10nrwRQxo_k",
                "admin_ok",
                "true",
            ),
            (
                "eyJ1c2VyX2lkIjozLCJ1c2VybmFtZSI6ImJvYiIsImFkbWluX29rIjp0cnVlfQ.awJLtw.wv1Q-DvToxq0wdVd0Bv51ZmH_-M",
                "username",
                "bob",
            ),
        ];
        for (tok, key, want) in cases {
            let d = loads_with_secret(tok, SECRET)
                .unwrap_or_else(|| panic!("Python 签发的 token 必须能验证: {tok}"));
            let got = d.get(*key).expect("字段存在");
            let got_s = match got {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            assert_eq!(got_s, *want, "token={tok} key={key}");
        }

        // 空载荷 token 也要能解（Python 的 {} 会话）
        let empty = loads_with_secret("e30.awJLtw.pZKED2xEQs-wMKunHQ-faQDxvIA", SECRET)
            .expect("空会话 token 应能验证");
        assert!(empty.is_empty());
    }

    #[test]
    fn rust_token_is_readable_by_python_format() {
        // Rust 签发的 token 必须是「三段 + base64url」，Python 才能读。
        // 键顺序不固定（HashMap），所以只断言「是紧凑 JSON、无空格、字段齐全」——
        // Python 侧 json.loads 不依赖顺序，兼容性不受影响。
        let mut s = SessionData::new();
        s.insert("user_id".into(), serde_json::json!(42));
        s.insert("username".into(), serde_json::json!("carol"));
        let tok = dumps_with_secret(&s, "shared-secret-for-test-1234567890ab");
        let parts: Vec<&str> = tok.split('.').collect();
        assert_eq!(parts.len(), 3, "token={tok}");

        use base64::Engine;
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[0])
            .unwrap();
        let text = String::from_utf8(raw).unwrap();
        assert!(!text.contains(' '), "必须是紧凑 JSON，否则 base64 与 Python 不一致: {text}");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["user_id"], 42);
        assert_eq!(v["username"], "carol");
    }
}
