//! 验证码提取：从邮件正文里找出 OTP。
//!
//! 这是整条链路最容易出错的地方。Python 版踩过的坑（全部保留在这里）：
//!
//! 1. **纯字母绝不是验证码**。关键词正则会把 `verification` + 空白 + `Hello`
//!    抓成验证码，实测线上返回过 `"Hello"`。
//! 2. **年份不是验证码**。`2019` 这类 4 位数不能当码。
//! 3. **HTML 里的颜色值**。`#1A2B3C` 会被 `[A-Z0-9]{6}` 误抓，要判上下文。
//! 4. **数字优先且 6 位加权**：多数站点是 6 位数字，8 位码存在但少见。
//!
//! 策略：先按「关键词附近」找，再退回全文扫描，取加权分数最高的候选。

use std::sync::OnceLock;

use regex::Regex;

/// 关键词附近的数字码（`验证码是 123456` / `code: 123456` / `OTP 654321`）
fn re_keyword_code() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"(?i)(验证码|校验码|动态码|安全码|确认码|code|passcode|otp|verification)[^\d<>]{0,40}((?:\d[\s-]*){4,8})",
        )
        .expect("正则已校验")
    })
}

/// 关键词附近的字母数字混合码（`code: A1B2C3`）
fn re_alnum_code() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"(?i)(?:code|passcode|otp|verification|验证码|校验码)[^\w]{0,20}([A-Z0-9]{3,8}-?[A-Z0-9]{0,8})",
        )
        .expect("正则已校验")
    })
}

/// 独立的 4-8 位连续数字（全文兜底）
fn re_bare_digits() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\b(\d{4,8})\b").expect("正则已校验"))
}

/// 会被误判成验证码的英文单词。
const CODE_STOP_WORDS: &[&str] = &[
    "verify", "verification", "email", "account", "click", "button", "below", "here", "team",
    "support", "help", "code", "passcode", "hello", "please", "thanks", "thank", "welcome",
    "login", "signin", "sign", "your", "this", "that", "with", "from", "have", "will", "http",
    "https", "www", "com", "net", "org", "html", "body", "div", "span", "style", "color",
];

/// 归一化文本：统一空白、去掉 HTML 标签、降低噪声。
pub fn normalize_text(value: &str) -> String {
    // 1. 去掉 <script> / <style> 整块（里面的代码不是验证码）
    let mut s = value.to_string();
    for tag in ["script", "style"] {
        let open = format!("<{tag}");
        while let Some(start) = s.to_ascii_lowercase().find(&open) {
            let Some(close_rel) = s[start..].to_ascii_lowercase().find(&format!("</{tag}>"))
            else {
                break;
            };
            let end = start + close_rel + tag.len() + 3;
            s.replace_range(start..end.min(s.len()), " ");
        }
    }
    // 2. HTML 标签 → 空白
    static TAG: OnceLock<Regex> = OnceLock::new();
    let tag = TAG.get_or_init(|| Regex::new(r"<[^>]{1,200}>").expect("正则已校验"));
    s = tag.replace_all(&s, " ").to_string();

    // 3. HTML 实体（最常见几个）
    s = s
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");

    // 4. 空白折叠
    static WS: OnceLock<Regex> = OnceLock::new();
    let ws = WS.get_or_init(|| Regex::new(r"[ \t\u{00a0}]+").expect("正则已校验"));
    ws.replace_all(&s, " ").trim().to_string()
}

/// 判断某个位置是否落在十六进制颜色值里（`#1A2B3C` / `color:#abc`）。
///
/// 实测：HTML 邮件里 `style="color:#1A2B3C"` 会被 `[A-Z0-9]{6}` 抓成码。
///
/// **注意**：`idx` 来自正则匹配的字节偏移，可能落在多字节字符中间。
/// 直接用 `&haystack[start..idx]` 会 panic（`byte index N is not a char
/// boundary`）——中文邮件必崩。所以这里先按字节做 ASCII 扫描，不切字符串。
fn is_hex_color_context(haystack: &str, idx: usize) -> bool {
    let bytes = haystack.as_bytes();
    let start = idx.saturating_sub(8);
    let window = &bytes[start..idx.min(bytes.len())];

    // 从右往左扫，只关心 ASCII（'#' 与 'rgb(' 都是 ASCII）
    let mut i = window.len();
    let mut hex_run = 0usize;
    while i > 0 {
        i -= 1;
        let b = window[i];
        if b.is_ascii_hexdigit() || b == b' ' || b == b':' {
            hex_run += 1;
            continue;
        }
        if b == b'#' && hex_run <= 7 {
            return true;
        }
        break;
    }
    // 挡住 `rgb(26, 43, 60)` 里的数字
    if window.windows(4).any(|w| w.eq_ignore_ascii_case(b"rgb(")) {
        return true;
    }
    false
}

/// 从纯文本提取验证码。返回空串 = 没找到。
pub fn extract_code(text: &str) -> String {
    let text = normalize_text(text);
    if text.is_empty() {
        return String::new();
    }

    // (候选, 分数)
    let mut candidates: Vec<(String, i32)> = Vec::new();

    // ① 关键词附近的纯数字：最高权重
    for cap in re_keyword_code().captures_iter(&text) {
        let raw = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
        if (4..=8).contains(&digits.len()) {
            // 年份不算
            if is_year(&digits) {
                continue;
            }
            let mut score = 100;
            if digits.len() == 6 {
                score += 40;
            }
            candidates.push((digits, score));
        }
    }

    // ② 关键词附近的字母数字混合
    for cap in re_alnum_code().captures_iter(&text) {
        let raw = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let code: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
        let code = code.trim_matches('-').to_string();
        if !(4..=10).contains(&code.len()) {
            continue;
        }
        // 必须含数字：纯字母绝不可能是验证码
        if !code.chars().any(|c| c.is_ascii_digit()) {
            continue;
        }
        if CODE_STOP_WORDS.contains(&code.to_ascii_lowercase().as_str()) {
            continue;
        }
        let mut score = 60;
        if code.len() == 6 {
            score += 20;
        }
        candidates.push((code, score));
    }

    // ③ 全文兜底：独立 4-8 位数字
    for cap in re_bare_digits().captures_iter(&text) {
        let Some(m) = cap.get(1) else { continue };
        let digits = m.as_str();
        if is_year(digits) {
            continue;
        }
        if is_hex_color_context(&text, m.start()) {
            continue;
        }
        // 6 位最像验证码；4/5 位次之；7/8 位再次
        let score = match digits.len() {
            6 => 50,
            4 | 5 => 30,
            7 | 8 => 20,
            _ => 0,
        };
        if score > 0 {
            candidates.push((digits.to_string(), score));
        }
    }

    // 取最高分；同分取先出现的（关键词区通常在前面）
    candidates
        .into_iter()
        .max_by_key(|(_, s)| *s)
        .map(|(c, _)| c)
        .unwrap_or_default()
}

/// 4 位数且像年份（1900-2099）→ 大概率是日期，不是验证码。
fn is_year(digits: &str) -> bool {
    if digits.len() != 4 {
        return false;
    }
    matches!(digits.parse::<u32>(), Ok(y) if (1900..=2099).contains(&y))
}

/// 从邮件 dict 提取验证码：优先用预提取字段，再回退全文。
pub fn extract_code_from_mail(mail: &serde_json::Value) -> String {
    for key in ["code", "code_match", "otp", "verification_code"] {
        if let Some(v) = mail.get(key) {
            if let Some(s) = v.as_str() {
                let s = s.trim();
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
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
    extract_code(&combined)
}

/// 项目级自定义正则：多行，每行一个。
///
/// 支持命名组 `(?P<code>...)` 指定验证码捕获组。
pub fn parse_code_patterns(value: &str) -> Vec<Regex> {
    value
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            // Rust 正则用 (?P<name>) 与 Python 一致
            Regex::new(l).ok()
        })
        .collect()
}

/// 用自定义正则提取；返回空串表示未命中。
pub fn extract_code_by_regex(text: &str, patterns: &[Regex]) -> String {
    for pat in patterns {
        if let Some(caps) = pat.captures(text) {
            // 优先命名组 code
            if let Some(m) = caps.name("code") {
                let v = m.as_str().trim();
                if !v.is_empty() {
                    return v.to_string();
                }
            }
            // 其次第一个捕获组
            if let Some(m) = caps.get(1) {
                let v = m.as_str().trim();
                if !v.is_empty() {
                    return v.to_string();
                }
            }
            // 最后整段匹配
            if let Some(m) = caps.get(0) {
                let v = m.as_str().trim();
                if !v.is_empty() {
                    return v.to_string();
                }
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_chinese_verification_code() {
        assert_eq!(extract_code("您的验证码是 483920，5 分钟内有效"), "483920");
        assert_eq!(extract_code("验证码：738291"), "738291");
    }

    #[test]
    fn finds_english_code() {
        assert_eq!(extract_code("Your verification code is 123456"), "123456");
        assert_eq!(extract_code("Enter this code: 908172"), "908172");
        assert_eq!(extract_code("Your OTP is 445566."), "445566");
    }

    #[test]
    fn ignores_hello_after_verification() {
        // 线上真实事故：`verification` + 空白 + `Hello` 被抓成验证码
        let got = extract_code("Email verification Hello welcome to our service");
        assert!(
            !got.eq_ignore_ascii_case("hello"),
            "绝不能把 Hello 当验证码，实际返回 {got:?}"
        );
    }

    #[test]
    fn ignores_pure_letters() {
        // 纯字母永远不是验证码
        for text in [
            "verification Hello",
            "your code Passcode",
            "Enter code VERIFY",
        ] {
            let got = extract_code(text);
            assert!(
                got.is_empty() || got.chars().any(|c| c.is_ascii_digit()),
                "{text:?} → {got:?}"
            );
        }
    }

    #[test]
    fn ignores_years() {
        assert_eq!(extract_code("Copyright 2026 Example Inc"), "");
        assert_eq!(extract_code("Since 1999 we serve"), "");
        // 但年份附近有关键词时，关键词分支也要挡住
        assert_eq!(extract_code("code from 2019"), "");
    }

    #[test]
    fn ignores_hex_colors() {
        // HTML 邮件里的颜色值
        let html = r#"<div style="color:#1A2B3C;background:#FFF">hi</div>"#;
        let got = extract_code(html);
        assert!(
            !got.eq_ignore_ascii_case("1a2b3c"),
            "十六进制颜色被误抓: {got:?}"
        );
    }

    #[test]
    fn strips_script_and_style() {
        let html = r#"<script>var x = 999999;</script><style>.a{color:#123456}</style>
                      您的验证码是 246810"#;
        assert_eq!(extract_code(html), "246810");
    }

    #[test]
    fn prefers_six_digit() {
        // 有 4 位和 6 位两个候选 → 6 位优先
        let got = extract_code("Order 1234, your code is 654321");
        assert_eq!(got, "654321");
    }

    #[test]
    fn alnum_code() {
        let got = extract_code("Your code: A1B2C3");
        assert_eq!(got, "A1B2C3");
    }

    #[test]
    fn empty_input() {
        assert_eq!(extract_code(""), "");
        assert_eq!(extract_code("   "), "");
        assert_eq!(extract_code("no code here at all"), "");
    }

    #[test]
    fn strips_html_entities() {
        let got = extract_code("<p>验证码&nbsp;是&nbsp;556677</p>");
        assert_eq!(got, "556677");
    }

    #[test]
    fn extract_from_mail_prefers_explicit_field() {
        let mail = serde_json::json!({"code": "111222", "body": "code is 999888"});
        assert_eq!(extract_code_from_mail(&mail), "111222");
    }

    #[test]
    fn extract_from_mail_falls_back_to_body() {
        let mail = serde_json::json!({"body": "Your code is 777888"});
        assert_eq!(extract_code_from_mail(&mail), "777888");
    }

    #[test]
    fn custom_regex_named_group() {
        let pats = parse_code_patterns(r"(?P<code>ZP-\d{6})");
        assert_eq!(pats.len(), 1);
        assert_eq!(extract_code_by_regex("your token ZP-123456 ok", &pats), "ZP-123456");
    }

    #[test]
    fn custom_regex_simple_group() {
        let pats = parse_code_patterns("验证码为(\\d{6})");
        assert_eq!(extract_code_by_regex("验证码为135790", &pats), "135790");
    }

    #[test]
    fn custom_regex_skips_comments_and_blanks() {
        let pats = parse_code_patterns("# 注释\n\n  (\\d{5})  \n[bad(");
        assert_eq!(pats.len(), 1, "只应保留 1 条合法正则");
    }
}
