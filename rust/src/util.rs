//! 通用工具：随机数、时间归一化、验证码提取。
//!
//! 时间语义与 Python `app/timeutil.py` 完全一致：
//! - 数据库 `DateTime` 列存 **naive UTC**
//! - Graph 返回 `2026-11-08T10:23:00Z`
//! - IMAP 返回 `Sat, 8 Nov 2026 10:23:00 +0800`
//! - 对外输出统一 ISO8601 **带时区**（UTC）

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};

/// 服务器时钟与上游可能的偏差容忍（秒）
pub const CLOCK_SLACK_SECONDS: i64 = 60;

// ────────────────────────────────────────────────────────────────
//  随机
// ────────────────────────────────────────────────────────────────

/// URL-safe 随机串（相当于 Python `secrets.token_urlsafe(n)` 的量级）。
pub fn random_token(nbytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; nbytes];
    rand::thread_rng().fill_bytes(&mut buf);
    base64_url_no_pad(&buf)
}

/// 十六进制随机串（相当于 Python `secrets.token_hex(n)`）。
pub fn random_hex(nbytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; nbytes];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// base64url 无填充（RFC 4648 §5）。
pub fn base64_url_no_pad(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// 生成 API Key：`ak_` + 32 位 hex。
pub fn generate_api_key() -> String {
    format!("ak_{}", random_hex(16))
}

/// 生成充值卡密（32 位 hex，无分隔符）。
pub fn generate_card_key() -> String {
    random_hex(16)
}

// ────────────────────────────────────────────────────────────────
//  时间
// ────────────────────────────────────────────────────────────────

/// 当前 UTC（aware）。
pub fn utc_now() -> DateTime<Utc> {
    Utc::now()
}

/// 当前 naive UTC（数据库存储用）。
pub fn now_naive() -> NaiveDateTime {
    Utc::now().naive_utc()
}

/// 任意时间表示 → 带时区 UTC。
///
/// 支持：naive datetime（按 UTC 解释）、ISO8601（含 Z）、RFC2822、unix 秒。
/// 无法解析返回 `None`。
pub fn normalize_utc(value: &str) -> Option<DateTime<Utc>> {
    let text = value.trim();
    if text.is_empty() {
        return None;
    }

    // ISO8601（含 Z / 偏移）
    if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
        return Some(dt.with_timezone(&Utc));
    }
    // 不带时区的 ISO8601 → 按 UTC
    if let Ok(ndt) = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(Utc.from_utc_datetime(&ndt));
    }
    if let Ok(ndt) = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f") {
        return Some(Utc.from_utc_datetime(&ndt));
    }
    // RFC2822（IMAP 的 Date 头）
    if let Ok(dt) = DateTime::parse_from_rfc2822(text) {
        return Some(dt.with_timezone(&Utc));
    }
    // 容错：真实邮件服务器的 Date 头经常把星期写错（如 2026-11-08 是周日
    // 却写成 "Sat"），chrono 会校验星期一致性并报 Impossible。
    // 剥掉开头的 "Www," 再按无星期格式解析一次。
    if let Some((_, rest)) = text.split_once(',') {
        if let Ok(dt) = DateTime::parse_from_str(rest.trim(), "%d %b %Y %H:%M:%S %z") {
            return Some(dt.with_timezone(&Utc));
        }
    }
    // unix 秒
    if let Ok(secs) = text.parse::<i64>() {
        if let Some(dt) = DateTime::from_timestamp(secs, 0) {
            return Some(dt);
        }
    }
    None
}

/// naive datetime（数据库值）→ UTC aware。数据库存的就是 UTC。
pub fn naive_to_utc(ndt: NaiveDateTime) -> DateTime<Utc> {
    Utc.from_utc_datetime(&ndt)
}

/// 数据库 naive UTC → 对外 ISO8601 字符串（带 `+00:00`）。
///
/// 对齐 Python `utc_iso()` + `datetime.isoformat()`：
/// - 时区用 **`+00:00`** 而不是 `Z`（`isoformat()` 就是这么输出的，
///   客户端如果做字符串比对/正则匹配会依赖这个形式）
/// - **微秒为 0 时省略小数部分**（`datetime(...,0).isoformat()` =
///   `2026-11-12T10:07:04+00:00`）
/// - 微秒非 0 时输出 6 位
pub fn iso_from_naive(ndt: Option<NaiveDateTime>) -> String {
    match ndt {
        Some(v) => {
            if v.and_utc().timestamp_subsec_micros() == 0 {
                format!("{}+00:00", v.format("%Y-%m-%dT%H:%M:%S"))
            } else {
                format!("{}+00:00", v.format("%Y-%m-%dT%H:%M:%S%.6f"))
            }
        }
        None => String::new(),
    }
}

/// 任意时间字符串 → 对外 ISO8601（带 `+00:00`）。
pub fn iso(value: &str) -> String {
    match normalize_utc(value) {
        Some(dt) => {
            let ndt = dt.naive_utc();
            iso_from_naive(Some(ndt))
        }
        None => String::new(),
    }
}

/// `value` 是否晚于 `reference`（允许 slack 秒误差）。任一无法解析 → false。
pub fn is_after(value: &str, reference: &str, slack_seconds: i64) -> bool {
    let (Some(a), Some(b)) = (normalize_utc(value), normalize_utc(reference)) else {
        return false;
    };
    a.timestamp() >= b.timestamp() - slack_seconds
}

/// 邮件时间是否「新鲜」（对齐 Python `is_fresh_mail`）。
///
/// 有基线时按基线判（防旧信），无基线时按 max_age 分钟。
pub fn is_fresh_mail(
    received_at: &str,
    baseline_at: Option<NaiveDateTime>,
    max_age_minutes: i64,
    slack_seconds: i64,
) -> bool {
    let Some(dt) = normalize_utc(received_at) else {
        return false;
    };
    match baseline_at {
        Some(base) => {
            let base = naive_to_utc(base);
            dt >= base - chrono::Duration::seconds(slack_seconds)
        }
        None => {
            let now = Utc::now();
            dt >= now - chrono::Duration::minutes(max_age_minutes)
        }
    }
}

// ────────────────────────────────────────────────────────────────
//  字符串 / 杂项
// ────────────────────────────────────────────────────────────────

/// 稳定哈希（用来做单进程内的 key，不要用于安全场景）。
pub fn stable_hash<T: Hash>(v: &T) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

/// 常量时间字符串比较（防时序侧信道，对齐 Python `secrets.compare_digest`）。
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    if ab.len() != bb.len() {
        return false;
    }
    ab.ct_eq(bb).into()
}

/// 掩码中间段（日志/页面展示用）：`abcdefg` → `abc•••efg`。
pub fn mask_middle(s: &str, head: usize, tail: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= head + tail {
        return "•".repeat(chars.len());
    }
    let h: String = chars[..head].iter().collect();
    let t: String = chars[chars.len() - tail..].iter().collect();
    format!("{h}{}{t}", "•".repeat(8))
}

/// 截断到指定字节数（按字符边界截，不会切坏 UTF-8）。
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
}

/// 解析客户端 IP：只信直连 peer，不信可伪造的 X-Forwarded-For。
///
/// （与 Python `client_host` 一致：反向代理场景由 uvicorn/axum 的
/// `--proxy-headers` 在监听层处理，应用层不再二次信任。）
pub fn is_loopback(addr: &str) -> bool {
    if addr.is_empty() {
        return false;
    }
    if addr == "localhost" || addr == "testclient" {
        return true;
    }
    let ip = addr.trim_matches(|c| c == '[' || c == ']');
    let ip = ip.split('%').next().unwrap_or(ip);
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_loopback(),
        Ok(std::net::IpAddr::V6(v6)) => v6.is_loopback(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_detection() {
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("::1"));
        assert!(is_loopback("[::1]"));
        assert!(is_loopback("localhost"));
        assert!(!is_loopback("45.205.27.41"));
        assert!(!is_loopback(""));
        assert!(!is_loopback("192.168.1.1"));
    }

    #[test]
    fn iso_always_has_offset() {
        let s = iso("2026-11-08T10:23:00Z");
        assert!(s.contains('+') || s.ends_with('Z'), "{s}");
        // naive 输入按 UTC 解释
        let s2 = iso("2026-11-08T10:23:00");
        assert!(s2.starts_with("2026-11-08T10:23:00"), "{s2}");
    }

    #[test]
    fn rfc2822_parsed() {
        let s = iso("Sat, 8 Nov 2026 10:23:00 +0800");
        assert!(s.starts_with("2026-11-08T02:23:00"), "{s}");
    }

    #[test]
    fn fresh_mail_uses_baseline() {
        let base = chrono::NaiveDate::from_ymd_opt(2026, 11, 8)
            .unwrap()
            .and_hms_opt(10, 0, 0)
            .unwrap();
        // 早于基线 30 分钟 → 旧信
        assert!(!is_fresh_mail("2026-11-08T09:30:00Z", Some(base), 120, 60));
        // 晚于基线 → 新信
        assert!(is_fresh_mail("2026-11-08T10:05:00Z", Some(base), 120, 60));
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq("secret", "secret"));
        assert!(!constant_time_eq("secret", "secreT"));
        assert!(!constant_time_eq("short", "longer"));
    }

    #[test]
    fn mask_is_short_safe() {
        assert_eq!(mask_middle("abc", 2, 2), "•••");
    }
}
