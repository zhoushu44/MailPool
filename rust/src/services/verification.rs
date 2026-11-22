//! 邮箱验证码：生成、校验、限流。
//!
//! ## 为什么不照抄 new-api
//!
//! 调研 new-api 源码后发现的 6 个缺陷，这里全部避开：
//!
//! | new-api 的做法 | 问题 | 这里的做法 |
//! |---|---|---|
//! | 验证码存进程内 RAM map | 多实例直接失效；重启丢失；无法审计 | 落库 `email_verification_codes` |
//! | 明文存 6 位 hex | 库泄露即可用；且 hex 含 a-f，用户体验差 | 6 位纯数字 + HMAC-SHA256 哈希 |
//! | 校验后不删除 | 10 分钟内可重复使用 | 校验成功立即删（consumed + 定时清理） |
//! | 无尝试次数上限 | 6 位码可暴力破解（100 万次） | 5 次失败即作废 |
//! | 只按 IP 限流 2 次/30 秒 | 换邮箱可无限发；换 IP 可轰炸同一邮箱 | IP + 邮箱双维度 |
//! | 无 `email_verified_at` | 分不清"填了邮箱"和"验证过邮箱" | users 表加该字段 |
//!
//! ## 为什么用 HMAC 而不是 Argon2
//!
//! 6 位数字只有 100 万种可能，Argon2 取 20ms 一次看似安全，但发码接口会
//! 变成 DoS 放大器（攻击者随便刷就能打满 CPU）。真正的防线是：
//! - 5 次尝试上限（爆破概率 5/1000000 ≈ 0.0005%）
//! - 10 分钟有效期
//! - 双维度限流（每小时最多发几次）
//!
//! HMAC 的密钥在进程内存里（来自 session secret），库泄露也解不出明文。

use chrono::{Duration, NaiveDateTime};
use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};
use crate::util::now_naive;

/// 验证码有效期（分钟）。对齐 new-api 的 `VerificationValidMinutes = 10`。
pub const CODE_TTL_MINUTES: i64 = 10;
/// 最大尝试次数。
pub const MAX_ATTEMPTS: i64 = 5;

/// 发信限流规则（滑动窗口）。
#[derive(Debug, Clone, Copy)]
pub struct RateLimit {
    /// 窗口长度（秒）
    pub window_secs: i64,
    /// 窗口内最多几条
    pub max: i64,
}

/// 同一邮箱：每小时最多 5 封（防止被轰炸 / 薅羊毛）。
pub const EMAIL_LIMIT: RateLimit = RateLimit {
    window_secs: 3600,
    max: 5,
};
/// 同一 IP：每小时最多 20 封（比邮箱宽松，一台机器可能有多个用户）。
pub const IP_LIMIT: RateLimit = RateLimit {
    window_secs: 3600,
    max: 20,
};
/// 同一邮箱最短重发间隔（秒）—— 防连点。
pub const RESEND_COOLDOWN_SECS: i64 = 60;

/// 验证码用途。
pub const PURPOSE_REGISTER: &str = "register";
pub const PURPOSE_RESET: &str = "reset";

/// 生成 6 位纯数字验证码。
///
/// **必须是纯数字**：new-api 用 `%06x` 生成 hex（含 a-f），用户从邮件里
/// 打字更容易错，而且很多前端输入框限制了 `inputmode=numeric`。
pub fn generate_code() -> String {
    use rand::Rng;
    let n: u32 = rand::thread_rng().gen_range(0..1_000_000);
    format!("{n:06}")
}

/// 计算验证码的 HMAC-SHA256（hex）。
///
/// 密钥来自 `MAILPOOL_SESSION_SECRET`：库文件和源码都泄露也推不出明文码，
/// 除非攻击者同时拿到运行时的环境变量。
fn hash_code(code: &str, email: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let secret = crate::config::Settings::get().session_secret.as_bytes();
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("任意长度都可做 HMAC key");
    // 把 email 也绑进去：同一个码不能跨邮箱复用
    mac.update(email.trim().to_ascii_lowercase().as_bytes());
    mac.update(b"|");
    mac.update(code.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// 限流检查结果。
#[derive(Debug, Clone)]
pub enum RateCheck {
    Ok,
    /// 被限流，附带还需等待的秒数
    Denied { reason: String, retry_after: i64 },
}

/// 检查发信限流（IP + 邮箱 双维度 + 重发冷却）。
pub async fn check_rate_limit(
    pool: &SqlitePool,
    email: &str,
    client_ip: &str,
) -> AppResult<RateCheck> {
    let email = email.trim().to_ascii_lowercase();
    let now = now_naive();

    // 1) 同一邮箱的重发冷却（防连点）
    if !email.is_empty() {
        let last: Option<NaiveDateTime> = sqlx::query_scalar(
            "SELECT MAX(created_at) FROM email_send_log WHERE email = ? AND ok = 1",
        )
        .bind(&email)
        .fetch_one(pool)
        .await?;
        if let Some(t) = last {
            let elapsed = (now - t).num_seconds();
            if elapsed < RESEND_COOLDOWN_SECS {
                return Ok(RateCheck::Denied {
                    reason: format!("请 {RESEND_COOLDOWN_SECS} 秒后再试"),
                    retry_after: RESEND_COOLDOWN_SECS - elapsed,
                });
            }
        }
    }

    // 2) 邮箱维度窗口
    if !email.is_empty() {
        let cutoff = now - Duration::seconds(EMAIL_LIMIT.window_secs);
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM email_send_log WHERE email = ? AND ok = 1 AND created_at > ?",
        )
        .bind(&email)
        .bind(cutoff)
        .fetch_one(pool)
        .await?;
        if n >= EMAIL_LIMIT.max {
            return Ok(RateCheck::Denied {
                reason: format!(
                    "该邮箱一小时内最多发送 {} 次",
                    EMAIL_LIMIT.max
                ),
                retry_after: EMAIL_LIMIT.window_secs,
            });
        }
    }

    // 3) IP 维度窗口
    if !client_ip.is_empty() {
        let cutoff = now - Duration::seconds(IP_LIMIT.window_secs);
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM email_send_log WHERE client_ip = ? AND ok = 1 AND created_at > ?",
        )
        .bind(client_ip)
        .bind(cutoff)
        .fetch_one(pool)
        .await?;
        if n >= IP_LIMIT.max {
            return Ok(RateCheck::Denied {
                reason: format!("当前网络一小时内最多发送 {} 次", IP_LIMIT.max),
                retry_after: IP_LIMIT.window_secs,
            });
        }
    }

    Ok(RateCheck::Ok)
}

/// 记一条发信日志（无论成功失败都记，便于排查）。
pub async fn log_send(
    pool: &SqlitePool,
    email: &str,
    client_ip: &str,
    purpose: &str,
    ok: bool,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO email_send_log (email, client_ip, purpose, ok, created_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(email.trim().to_ascii_lowercase())
    .bind(crate::util::truncate_chars(client_ip, 64))
    .bind(purpose)
    .bind(if ok { 1 } else { 0 })
    .bind(now_naive())
    .execute(pool)
    .await?;
    Ok(())
}

/// 签发一个新验证码（作废旧码，写新的）。
///
/// 返回明文码（只在这一刻存在，随后只剩哈希）。
pub async fn issue_code(
    pool: &SqlitePool,
    email: &str,
    purpose: &str,
    client_ip: &str,
) -> AppResult<String> {
    let email = email.trim().to_ascii_lowercase();
    if email.is_empty() || !email.contains('@') {
        return Err(AppError::bad_request("邮箱格式不正确"));
    }

    let code = generate_code();
    let hash = hash_code(&code, &email);
    let now = now_naive();
    let expires = now + Duration::minutes(CODE_TTL_MINUTES);

    // 同一邮箱同一用途只保留最新一个码：作废旧码，
    // 否则用户可以同时持有多个有效码（增大爆破面）
    sqlx::query(
        "DELETE FROM email_verification_codes WHERE email = ? AND purpose = ? AND consumed = 0",
    )
    .bind(&email)
    .bind(purpose)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO email_verification_codes \
         (email, code_hash, purpose, attempts, max_attempts, expires_at, consumed, client_ip, created_at) \
         VALUES (?, ?, ?, 0, ?, ?, 0, ?, ?)",
    )
    .bind(&email)
    .bind(&hash)
    .bind(purpose)
    .bind(MAX_ATTEMPTS)
    .bind(expires)
    .bind(crate::util::truncate_chars(client_ip, 64))
    .bind(now)
    .execute(pool)
    .await?;

    Ok(code)
}

/// 校验验证码。**成功即删除**（防重放）。
///
/// 返回 `(ok, 错误原因)`。
pub async fn verify_code(
    pool: &SqlitePool,
    email: &str,
    code: &str,
    purpose: &str,
) -> AppResult<(bool, String)> {
    let email = email.trim().to_ascii_lowercase();
    let code = code.trim();

    // 行结构：(id, code_hash, attempts, max_attempts, expires_at)
    let row: Option<(i64, String, i64, i64, NaiveDateTime)> = sqlx::query_as(
        "SELECT id, code_hash, attempts, max_attempts, expires_at \
         FROM email_verification_codes \
         WHERE email = ? AND purpose = ? AND consumed = 0 \
         ORDER BY id DESC LIMIT 1",
    )
    .bind(&email)
    .bind(purpose)
    .fetch_optional(pool)
    .await?;

    let Some((id, stored_hash, attempts, max_attempts, expires_at)) = row else {
        return Ok((false, "验证码不存在或已过期".to_string()));
    };

    // 过期
    if now_naive() > expires_at {
        sqlx::query("DELETE FROM email_verification_codes WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        return Ok((false, "验证码已过期".to_string()));
    }

    // 尝试次数上限
    if attempts >= max_attempts {
        sqlx::query("DELETE FROM email_verification_codes WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        return Ok((false, "验证码错误次数过多，请重新获取".to_string()));
    }

    // 比对（常量时间）
    let given_hash = hash_code(code, &email);
    if !crate::util::constant_time_eq(&given_hash, &stored_hash) {
        // 失败：累加次数
        let new_attempts = attempts + 1;
        if new_attempts >= max_attempts {
            // **达到上限立即删除**（而不是等下一次请求进来才删）。
            // 原实现只在下次请求开头检查 attempts >= max 才删，
            // 结果是「第 5 次失败后码还躺在库里」，多留了一个攻击窗口。
            sqlx::query("DELETE FROM email_verification_codes WHERE id = ?")
                .bind(id)
                .execute(pool)
                .await?;
            return Ok((false, "验证码错误次数过多，请重新获取".to_string()));
        }
        sqlx::query("UPDATE email_verification_codes SET attempts = ? WHERE id = ?")
            .bind(new_attempts)
            .bind(id)
            .execute(pool)
            .await?;
        let left = max_attempts - new_attempts;
        return Ok((
            false,
            format!("验证码错误，还可尝试 {left} 次"),
        ));
    }

    // 成功：**立即删除**（new-api 不删，10 分钟内可复用）
    sqlx::query("DELETE FROM email_verification_codes WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;

    Ok((true, String::new()))
}

/// 清理过期验证码（定时任务调）。
pub async fn cleanup_expired(pool: &SqlitePool) -> AppResult<i64> {
    let res = sqlx::query("DELETE FROM email_verification_codes WHERE expires_at < ?")
        .bind(now_naive())
        .execute(pool)
        .await?;
    Ok(res.rows_affected() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_six_digits() {
        for _ in 0..200 {
            let c = generate_code();
            assert_eq!(c.len(), 6, "必须是 6 位: {c}");
            assert!(
                c.chars().all(|ch| ch.is_ascii_digit()),
                "必须是纯数字（new-api 的 hex 是错的）: {c}"
            );
            // 允许前导零（007721 是合法的）
        }
    }

    #[test]
    fn code_distribution_not_degenerate() {
        // 简单校验随机性：1000 次里应该有大量不同的值
        let set: std::collections::HashSet<String> =
            (0..1000).map(|_| generate_code()).collect();
        assert!(set.len() > 900, "随机性太差: 只有 {} 个唯一值", set.len());
    }

    #[test]
    fn hash_is_stable_and_email_bound() {
        let h1 = hash_code("123456", "a@b.test");
        let h2 = hash_code("123456", "a@b.test");
        assert_eq!(h1, h2, "同一输入必须得到同一哈希");

        // 同一个码换个邮箱 → 哈希必须不同（防跨邮箱复用）
        let h3 = hash_code("123456", "other@b.test");
        assert_ne!(h1, h3, "验证码必须绑定邮箱");

        assert_eq!(h1.len(), 64, "SHA256 hex 是 64 字符");
    }

    #[test]
    fn hash_is_case_insensitive_on_email() {
        assert_eq!(
            hash_code("123456", "A@B.TEST"),
            hash_code("123456", "a@b.test"),
            "邮箱大小写不该影响校验"
        );
    }
}
