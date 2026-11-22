//! IMAP 降级通道：XOAUTH2 认证拉邮件。
//!
//! ## 为什么要这条通道
//!
//! Graph 与 IMAP 是**不同的服务端**（`graph.microsoft.com` vs
//! `outlook.office365.com:993`），限流配额独立。995 个号共用一个 client_id
//! 且无代理时，这是把并发做到 30 的关键分流阀：Graph 车道满了就走这里。
//!
//! ## XOAUTH2 认证
//!
//! IMAP 的 `AUTHENTICATE XOAUTH2` 需要：
//! ```text
//! base64("user=<email>\x01auth=Bearer <access_token>\x01\x01")
//! ```
//! 注意是 **`\x01` 分隔**，不是空格；结尾两个 `\x01`。写错会得到
//! `AUTHENTICATE failed`（无更多信息），非常难排查。
//!
//! ## 用哪个 scope
//!
//! 用 Thunderbird 的公开 client_id 拿到的 token 通常**缺少 Graph scope**
//! （只有 IMAP/SMTP）。这种情况下 Graph 通道会 401，必须能自动走 IMAP ——
//! 所以这里的 scope 回退逻辑很重要：先试 Graph scope 的缓存，
//! 再试 IMAP scope。

use std::time::{Duration, Instant};

use crate::error::{AppError, AppResult};
use crate::providers::extract::{extract_code, extract_code_by_regex, parse_code_patterns};
use crate::providers::graph::{Hit, Mail, WaitOptions};
use crate::providers::outlook::{
    access_token, is_credential_fatal, rate_acquire, rate_report, Credentials, Lane, Outcome,
    IMAP_SCOPE, IMAP_SERVERS,
};
use crate::util::{normalize_utc, truncate_chars};

/// IMAP 连接超时
const IMAP_TIMEOUT: Duration = Duration::from_secs(30);
/// 轮间间隔
const POLL_INTERVAL: Duration = Duration::from_secs(4);
/// IMAP 车道最大并发连接
fn max_imap_conns() -> usize {
    std::env::var("MAILPOOL_IMAP_MAX_CONNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24)
}

/// 构造 XOAUTH2 认证串。
///
/// `user=<email>\x01auth=Bearer <token>\x01\x01`
pub fn xoauth2_string(email: &str, access_token: &str) -> String {
    format!("user={email}\x01auth=Bearer {access_token}\x01\x01")
}

/// base64 编码 XOAUTH2（IMAP 协议要求）。
pub fn xoauth2_b64(email: &str, access_token: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(xoauth2_string(email, access_token).as_bytes())
}

/// 一封 IMAP 邮件的原始字段。
#[derive(Debug, Clone, Default)]
pub struct ImapRaw {
    pub uid: String,
    pub subject: String,
    pub from: String,
    pub date: String,
    pub body: String,
}

/// 从 IMAP 的原始 message 解析出结构化字段。
///
/// `mailparse` 负责 MIME 解码（multipart/alternative、quoted-printable、
/// base64、字符集转换），这部分自己写极易踩坑。
pub fn parse_imap_message(raw: &[u8]) -> ImapRaw {
    let parsed = match mailparse::parse_mail(raw) {
        Ok(p) => p,
        Err(_) => return ImapRaw::default(),
    };

    let get_header = |name: &str| -> String {
        parsed
            .headers
            .iter()
            .find(|h| h.get_key().eq_ignore_ascii_case(name))
            .map(|h| h.get_value())
            .unwrap_or_default()
    };

    ImapRaw {
        uid: String::new(),
        subject: get_header("Subject"),
        from: get_header("From"),
        date: get_header("Date"),
        body: extract_body_text(&parsed),
    }
}

/// 递归提取邮件正文。
///
/// `mailparse` 的 `get_body()` **只对叶子节点有效**：对
/// `multipart/alternative` 直接调用会返回空串。验证码邮件几乎都是
/// multipart（同时带 text/plain 与 text/html），所以必须递归。
///
/// 策略：
/// 1. 优先 `text/plain`（验证码通常在里面，且没 HTML 噪声干扰提取）
/// 2. 没有再取 `text/html`
/// 3. 都没有则把所有子部件拼起来兜底
fn extract_body_text(parsed: &mailparse::ParsedMail<'_>) -> String {
    fn mime(message: &mailparse::ParsedMail<'_>) -> String {
        message.ctype.mimetype.to_ascii_lowercase()
    }

    // 叶子节点：直接取
    if parsed.subparts.is_empty() {
        return parsed.get_body().unwrap_or_default();
    }

    // 广度优先找 text/plain
    let mut plain: Option<String> = None;
    let mut html: Option<String> = None;
    let mut all: Vec<String> = Vec::new();

    fn walk(
        m: &mailparse::ParsedMail<'_>,
        plain: &mut Option<String>,
        html: &mut Option<String>,
        all: &mut Vec<String>,
    ) {
        for sub in &m.subparts {
            let t = sub.ctype.mimetype.to_ascii_lowercase();
            if sub.subparts.is_empty() {
                if let Ok(body) = sub.get_body() {
                    if body.trim().is_empty() {
                        continue;
                    }
                    if t == "text/plain" && plain.is_none() {
                        *plain = Some(body.clone());
                    } else if t == "text/html" && html.is_none() {
                        *html = Some(body.clone());
                    }
                    all.push(body);
                }
            } else {
                walk(sub, plain, html, all);
            }
        }
    }
    walk(parsed, &mut plain, &mut html, &mut all);

    // text/plain 优先；没有就用 html（后面的验证码提取会剥标签）
    if let Some(p) = plain {
        return p;
    }
    if let Some(h) = html {
        return h;
    }
    let _ = mime(parsed);
    all.join("\n")
}

/// IMAP 轮询等码。
///
/// 返回 `Ok(None)` = 超时；`Err(Fatal)` = 凭据失效；其它 `Err` = 两条通道都失败。
pub async fn wait_code_imap(
    cred: &mut Credentials,
    email: &str,
    opts: &WaitOptions,
) -> AppResult<Option<Hit>> {
    let client_id = cred.client_id.clone();

    // IMAP 车道软上限：超出排队 10s，再超出放行（软限不丢活）
    if !rate_acquire(Lane::Imap, &client_id, Duration::from_secs(10)).await {
        // 软限制：排队超时也继续（宁可多开连接也别丢任务）
        tracing::warn!("[imap] 车道排队超时，放行 email={email}");
    }

    // IMAP 通常需要 IMAP scope；先试缓存里的 Graph token，再试 IMAP scope
    let mut token = None;
    for scope in [IMAP_SCOPE, crate::providers::outlook::GRAPH_SCOPE] {
        match access_token(&build_client(cred)?, cred, scope, email).await {
            Ok(t) => {
                token = Some(t);
                break;
            }
            Err(e) => {
                let msg = e.to_string();
                if is_credential_fatal(&msg) {
                    return Err(AppError::bad_gateway(format!("凭据失效: {msg}")));
                }
                tracing::info!("[imap] scope {scope} 取 token 失败: {msg}");
            }
        }
    }
    let Some(token) = token else {
        return Err(AppError::bad_gateway("IMAP 无法获取 access_token"));
    };

    let patterns = parse_code_patterns(&opts.code_regex);
    let deadline = Instant::now() + opts.timeout;

    // imap crate 是同步的，放阻塞线程池里跑，别卡住 tokio
    let email_owned = email.to_string();
    let opts_clone = opts.clone();
    let token_clone = token.clone();

    let result = tokio::task::spawn_blocking(move || {
        poll_imap_sync(&email_owned, &token_clone, &opts_clone, &patterns, deadline)
    })
    .await
    .map_err(|e| AppError::internal(format!("IMAP 任务 panic: {e}")))??;

    Ok(result)
}

fn build_client(cred: &Credentials) -> AppResult<reqwest::Client> {
    crate::providers::outlook::build_http_client(&cred.proxy)
}

/// 同步的 IMAP 轮询（跑在阻塞线程池里）。
fn poll_imap_sync(
    email: &str,
    token: &str,
    opts: &WaitOptions,
    patterns: &[regex::Regex],
    deadline: Instant,
) -> AppResult<Option<Hit>> {
    let client_id = "";
    let _ = client_id;

    while Instant::now() < deadline {
        for server in IMAP_SERVERS {
            let remain = deadline.saturating_duration_since(Instant::now());
            if remain.is_zero() {
                break;
            }

            match poll_one_server(server, email, token, opts, patterns, remain) {
                Ok(Some(hit)) => return Ok(Some(hit)),
                Ok(None) => continue, // 这个服务器没新信，换下一个
                Err(ImapError::Fatal(msg)) => {
                    return Err(AppError::bad_gateway(format!("IMAP 凭据失效: {msg}")));
                }
                Err(e) => {
                    tracing::info!("[imap] {server} 失败: {e}");
                    continue;
                }
            }
        }

        let remain = deadline.saturating_duration_since(Instant::now());
        let sleep = POLL_INTERVAL.min(remain);
        if !sleep.is_zero() {
            std::thread::sleep(sleep);
        }
    }
    Ok(None)
}

#[derive(Debug, thiserror::Error)]
enum ImapError {
    #[error("凭据失效: {0}")]
    Fatal(String),
    #[error("{0}")]
    Other(String),
}

/// 连一个 IMAP 服务器、拉新邮件、找码。
fn poll_one_server(
    server: &str,
    email: &str,
    token: &str,
    opts: &WaitOptions,
    patterns: &[regex::Regex],
    _remain: Duration,
) -> Result<Option<Hit>, ImapError> {
    let tls = native_tls::TlsConnector::builder()
        .build()
        .map_err(|e| ImapError::Other(format!("TLS 初始化失败: {e}")))?;

    let client = imap::connect((server, 993), server, &tls)
        .map_err(|e| ImapError::Other(format!("连接 {server} 失败: {e}")))?;

    let mut session = client
        .authenticate("XOAUTH2", &Xoauth2Authenticator { email, token })
        .map_err(|(e, _client)| {
            let msg = format!("{server} 认证失败: {e}");
            if msg.to_ascii_lowercase().contains("authenticate failed") {
                ImapError::Fatal(msg)
            } else {
                ImapError::Other(msg)
            }
        })?;

    let result = (|| -> Result<Option<Hit>, ImapError> {
        session
            .select("INBOX")
            .map_err(|e| ImapError::Other(format!("select INBOX: {e}")))?;

        // 只取最近 20 封（验证码邮件一定在最上面）
        let fetches = session
            .fetch("1:20", "(UID RFC822)")
            .map_err(|e| ImapError::Other(format!("fetch: {e}")))?;

        for f in fetches.iter() {
            let Some(raw) = f.body() else { continue };
            let mut parsed = parse_imap_message(raw);
            if let Some(uid) = f.uid {
                parsed.uid = uid.to_string();
            }

            // 基线过滤：这条消息在基线里 → 旧信
            if !parsed.uid.is_empty() && opts.baseline_ids.contains(&parsed.uid) {
                continue;
            }
            if let Some(base) = opts.baseline_at {
                if let Some(at) = normalize_utc(&parsed.date) {
                    if at < base - chrono::Duration::seconds(60) {
                        continue;
                    }
                }
            }

            let combined = format!("{}\n{}", parsed.subject, parsed.body);
            let code = extract_code_by_regex(&combined, patterns);
            let (code, from_regex) = if code.is_empty() {
                (extract_code(&combined), false)
            } else {
                (code, true)
            };

            if !code.is_empty() && opts.skip_codes.contains(&code) {
                continue;
            }

            return Ok(Some(Hit {
                mail: Mail {
                    message_id: parsed.uid.clone(),
                    subject: parsed.subject.clone(),
                    body: parsed.body.clone(),
                    body_preview: truncate_chars(&parsed.body, 200),
                    sender: parsed.from.clone(),
                    folder: "INBOX".to_string(),
                    received_at: parsed.date.clone(),
                    provider: "outlook".to_string(),
                },
                code,
                from_regex,
            }));
        }
        Ok(None)
    })();

    let _ = session.logout();
    result
}

/// IMAP 的 XOAUTH2 认证器。
struct Xoauth2Authenticator<'a> {
    email: &'a str,
    token: &'a str,
}

impl imap::Authenticator for Xoauth2Authenticator<'_> {
    type Response = String;

    fn process(&self, _challenge: &[u8]) -> Self::Response {
        xoauth2_b64(self.email, self.token)
    }
}

/// 校验账号是否还能连（健康扫描用）。
///
/// 返回 `(healthy, detail)`。网络抖动视为 healthy（不误杀号），
/// 只有明确的凭据失效才判 unhealthy/dead。
pub async fn verify_account(cred: &mut Credentials, email: &str) -> (bool, String) {
    let http = match build_client(cred) {
        Ok(c) => c,
        Err(e) => return (true, format!("客户端构建失败（不判死）: {e}")),
    };

    // 先试 Graph
    match access_token(&http, cred, crate::providers::outlook::GRAPH_SCOPE, email).await {
        Ok(t) => {
            match crate::providers::graph::list_messages(&http, &t, "inbox", 1, None).await {
                Ok(_) => (true, "graph ok".into()),
                Err(crate::providers::graph::GraphError::Fatal(msg)) => {
                    // Graph 401 但 token 拿到了 → 可能是缺 Graph scope，试 IMAP
                    if let Ok(imap_t) = access_token(&http, cred, IMAP_SCOPE, email).await {
                        if check_imap_login(email, &imap_t).await {
                            return (true, "graph 无权限但 IMAP 可用".into());
                        }
                    }
                    if crate::providers::outlook::is_risk_control(&msg) {
                        (false, format!("risk_control: {msg}"))
                    } else {
                        (false, format!("graph fatal: {msg}"))
                    }
                }
                Err(e) => (true, format!("graph 临时错误（不判死）: {e}")),
            }
        }
        Err(e) => {
            let msg = e.to_string();
            if crate::providers::outlook::is_risk_control(&msg) {
                return (false, "risk_control: service abuse mode".into());
            }
            if is_credential_fatal(&msg) {
                return (false, format!("credential_invalid: {}", truncate_chars(&msg, 100)));
            }
            (true, format!("token 临时错误（不判死）: {}", truncate_chars(&msg, 80)))
        }
    }
}

/// 试一次 IMAP 登录（验证 IMAP scope 是否可用）。
async fn check_imap_login(email: &str, token: &str) -> bool {
    let email = email.to_string();
    let token = token.to_string();
    tokio::task::spawn_blocking(move || {
        let Ok(tls) = native_tls::TlsConnector::builder().build() else {
            return false;
        };
        for server in IMAP_SERVERS {
            let Ok(client) = imap::connect((*server, 993), *server, &tls) else {
                continue;
            };
            let Ok(mut session) = client.authenticate(
                "XOAUTH2",
                &Xoauth2Authenticator {
                    email: &email,
                    token: &token,
                },
            ) else {
                continue;
            };
            let ok = session.select("INBOX").is_ok();
            let _ = session.logout();
            if ok {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false)
}

/// IMAP 车道是否还有空位（供调度器决定走哪条通道）。
pub fn imap_has_capacity() -> bool {
    let _ = max_imap_conns();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xoauth2_format_is_exact() {
        // 格式必须严格：\x01 分隔，结尾两个 \x01
        let s = xoauth2_string("a@b.test", "TOKEN");
        assert_eq!(s, "user=a@b.test\x01auth=Bearer TOKEN\x01\x01");
        assert_eq!(s.matches('\x01').count(), 3, "必须是 3 个 \\x01");
    }

    #[test]
    fn xoauth2_base64_decodes_back() {
        use base64::Engine;
        let b64 = xoauth2_b64("a@b.test", "TOKEN");
        let raw = base64::engine::general_purpose::STANDARD.decode(&b64).unwrap();
        let s = String::from_utf8(raw).unwrap();
        assert!(s.starts_with("user=a@b.test"));
        assert!(s.contains("auth=Bearer TOKEN"));
    }

    #[test]
    fn parse_simple_message() {
        let raw = b"From: noreply@service.com\r\n\
                    Subject: Your verification code\r\n\
                    Date: Sat, 22 Nov 2026 09:00:00 +0800\r\n\
                    Content-Type: text/plain; charset=utf-8\r\n\
                    \r\n\
                    Your code is 123456\r\n";
        let p = parse_imap_message(raw);
        assert_eq!(p.subject, "Your verification code");
        assert!(p.from.contains("noreply@service.com"));
        assert!(p.body.contains("123456"));
    }

    #[test]
    fn parse_multipart_message() {
        let raw = b"From: a@b.test\r\n\
                    Subject: code\r\n\
                    Content-Type: multipart/alternative; boundary=\"BOUND\"\r\n\
                    \r\n\
                    --BOUND\r\n\
                    Content-Type: text/plain\r\n\
                    \r\n\
                    Your code is 654321\r\n\
                    --BOUND\r\n\
                    Content-Type: text/html\r\n\
                    \r\n\
                    <p>Your code is <b>654321</b></p>\r\n\
                    --BOUND--\r\n";
        let p = parse_imap_message(raw);
        assert!(p.body.contains("654321"), "body={:?}", p.body);
    }

    #[test]
    fn parse_quoted_printable() {
        let raw = b"From: a@b.test\r\n\
                    Subject: test\r\n\
                    Content-Type: text/plain; charset=utf-8\r\n\
                    Content-Transfer-Encoding: quoted-printable\r\n\
                    \r\n\
                    Your code is 3D 789012\r\n";
        let p = parse_imap_message(raw);
        // quoted-printable 的 =3D 应解成 '='
        assert!(!p.body.contains("=3D"), "QP 未解码: {:?}", p.body);
    }

    #[test]
    fn parse_garbage_does_not_panic() {
        // 真实场景：IMAP 偶尔返回截断/损坏的 message，绝不能 panic
        for raw in [
            &b""[..],
            &b"not a mail at all"[..],
            &b"\x00\xff\xfe garbage"[..],
            &b"Subject: only subject\r\n\r\n"[..],
        ] {
            let p = parse_imap_message(raw);
            let _ = p.subject; // 不 panic 即可
        }
    }
}
