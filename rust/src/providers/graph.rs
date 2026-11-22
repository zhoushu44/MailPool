//! Graph API 通道：列邮件、取正文、轮询等码。
//!
//! 设计要点（对齐 Python `outlook.py` 的 `_graph_wait_code`）：
//!
//! 1. **只扫 inbox + junkemail**。实测验证码几乎只落 inbox，扫 3 个文件夹
//!    是 3 倍请求量；junk 兜底防止被判垃圾。
//! 2. **轮间 sleep 1s**（不是 4s）。12s 窗口内轮次从 2.9 提到 10.7。
//! 3. **429/503 只退避不 raise**。raise 会让该账号 Graph 通道当场死亡，
//!    整批挤进 IMAP 降级 → 雪崩到 230s。
//! 4. **连续 3 整轮 5xx 才降级 IMAP**。单次抖动不降级。
//! 5. **基线去重**：只返回 `received_at >= baseline_at` 的新信，
//!    否则回收号里的 8~14 天前旧码会被当成新码。

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::providers::extract::{extract_code, extract_code_by_regex, parse_code_patterns};
use crate::providers::outlook::{
    access_token, bucket_for, is_credential_fatal, rate_acquire, rate_penalize, rate_report,
    Credentials, Lane, Outcome, GRAPH_BASE, GRAPH_FOLDERS, GRAPH_SCOPE,
};
use crate::util::{iso, normalize_utc, truncate_chars};

/// 单次 Graph 请求超时
const GRAPH_TIMEOUT: Duration = Duration::from_secs(15);
/// 硬超时（响应变慢是限流前兆）
const HARD_TIMEOUT: Duration = Duration::from_secs(8);
/// 轮间间隔
const POLL_INTERVAL: Duration = Duration::from_millis(1000);
/// 慢轮间隔（上一轮耗时超过 3s 时放缓）
const SLOW_POLL_INTERVAL: Duration = Duration::from_millis(3000);
/// 连续多少整轮 5xx 才降级 IMAP
const MAX_BAD_ROUNDS: u32 = 3;
/// Graph 车道最大同时轮询数
fn max_graph_pollers() -> usize {
    std::env::var("MAILPOOL_GRAPH_MAX_POLLERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12)
}

/// 一条邮件的标准化结构（Graph / IMAP 都归一到这里）。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Mail {
    pub message_id: String,
    pub subject: String,
    pub body: String,
    pub body_preview: String,
    pub sender: String,
    pub folder: String,
    pub received_at: String,
    pub provider: String,
}

/// 带硬超时的请求执行。
///
/// `reqwest` 的 timeout 只管单次 socket 读写：服务器握手后慢慢滴数据时，
/// 一个请求能拖到 60s+（实测 Graph 查询 604s）。这里强制封顶。
async fn with_hard_timeout<F, T>(fut: F, limit: Duration) -> Result<T, GraphError>
where
    F: std::future::Future<Output = Result<T, GraphError>>,
{
    match tokio::time::timeout(limit, fut).await {
        Ok(r) => r,
        Err(_) => Err(GraphError::Slow),
    }
}

/// Graph 调用错误分类（决定是退避、换通道、还是判号废）。
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error("限流 {0}")]
    Throttled(u16),
    #[error("服务端错误 {0}")]
    Server(u16),
    #[error("响应变慢")]
    Slow,
    #[error("凭据失效: {0}")]
    Fatal(String),
    #[error("网络错误: {0}")]
    Network(String),
    #[error("{0}")]
    Other(String),
}

impl GraphError {
    /// 是否属于「这一整轮都失败」（用于计数降级）。
    fn is_round_failure(&self) -> bool {
        matches!(self, GraphError::Server(_) | GraphError::Other(_))
    }

    fn is_throttle(&self) -> bool {
        matches!(self, GraphError::Throttled(_) | GraphError::Server(_))
    }
}

/// 列某文件夹的邮件（只取需要的字段，减少传输）。
///
/// `with_body` 决定是否取正文：
/// - **列表页不取**（`$select` 里没有 body）—— 正文动辄几十 KB，
///   20 封就是 1MB+，列表页根本用不到（只显示预览）
/// - 取码时**要取**（否则提取不到验证码）
///
/// 这个差异是从 Python 版对齐来的：它的 `graph_list_messages` 也分两种
/// `$select`。
pub async fn list_messages(
    http: &reqwest::Client,
    access_token: &str,
    folder: &str,
    top: u32,
    since: Option<&str>,
) -> Result<Vec<Value>, GraphError> {
    list_messages_opts(http, access_token, folder, top, since, true).await
}

/// 带 `with_body` 开关的版本。
pub async fn list_messages_opts(
    http: &reqwest::Client,
    access_token: &str,
    folder: &str,
    top: u32,
    since: Option<&str>,
    with_body: bool,
) -> Result<Vec<Value>, GraphError> {
    let select = if with_body {
        "id,subject,bodyPreview,receivedDateTime,from,toRecipients,isRead,body"
    } else {
        "id,subject,bodyPreview,receivedDateTime,from,toRecipients,isRead"
    };
    let mut url = format!(
        "{GRAPH_BASE}/me/mailFolders/{folder}/messages?$top={top}\
         &$select={select}\
         &$orderby=receivedDateTime desc"
    );
    if let Some(s) = since {
        // OData 的日期字面量不能带毫秒以外的精度问题，统一用 ISO8601
        let escaped = s.replace('\'', "''");
        url.push_str(&format!(
            "&$filter=receivedDateTime ge {}",
            escaped
        ));
    }

    let resp = http
        .get(&url)
        .bearer_auth(access_token)
        .timeout(GRAPH_TIMEOUT)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                GraphError::Slow
            } else {
                GraphError::Network(e.to_string())
            }
        })?;

    let status = resp.status();
    if status.as_u16() == 429 {
        return Err(GraphError::Throttled(429));
    }
    if status.is_server_error() {
        return Err(GraphError::Server(status.as_u16()));
    }
    if status.as_u16() == 401 {
        let text = resp.text().await.unwrap_or_default();
        return Err(GraphError::Fatal(format!("401 {text}")));
    }
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(GraphError::Other(format!(
            "HTTP {status}: {}",
            truncate_chars(&text, 200)
        )));
    }

    let body: Value = resp
        .json()
        .await
        .map_err(|e| GraphError::Other(format!("响应非 JSON: {e}")))?;
    Ok(body
        .get("value")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default())
}

/// 把 Graph 的 message 对象归一成 [`Mail`]。
pub fn normalize_message(msg: &Value) -> Mail {
    let sender = msg
        .get("from")
        .and_then(|f| f.get("emailAddress"))
        .and_then(|e| e.get("address"))
        .and_then(|a| a.as_str())
        .unwrap_or("")
        .to_string();
    let body = msg
        .get("body")
        .and_then(|b| b.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    Mail {
        message_id: msg
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        subject: msg
            .get("subject")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        body_preview: msg
            .get("bodyPreview")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        body,
        sender,
        folder: String::new(),
        received_at: msg
            .get("receivedDateTime")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default(),
        provider: "outlook".to_string(),
    }
}

/// 轮询等码的参数。
#[derive(Debug, Clone)]
pub struct WaitOptions {
    /// 总超时
    pub timeout: Duration,
    /// 取号瞬间的收件箱基线（message id 集合），用于挡旧信
    pub baseline_ids: std::collections::HashSet<String>,
    /// 基线时间（只认这之后收到的信）
    pub baseline_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 已返回过的验证码（跳过，防重复返回同一个码）
    pub skip_codes: std::collections::HashSet<String>,
    /// 项目级自定义提取正则（多行）
    pub code_regex: String,
}

impl Default for WaitOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            baseline_ids: std::collections::HashSet::new(),
            baseline_at: None,
            skip_codes: std::collections::HashSet::new(),
            code_regex: String::new(),
        }
    }
}

/// 一封命中的邮件 + 提取出的码。
#[derive(Debug, Clone)]
pub struct Hit {
    pub mail: Mail,
    pub code: String,
    /// 是否用项目自定义正则提取
    pub from_regex: bool,
}

/// Graph 通道轮询等码。
///
/// 返回 `Ok(None)` = 超时没等到（**正常轮询结果，不是错误**）；
/// 返回 `Err(GraphError::Fatal)` = 凭据失效（该号作废）；
/// 其它 `Err` = 应降级到 IMAP。
pub async fn wait_code_graph(
    cred: &mut Credentials,
    email: &str,
    opts: &WaitOptions,
) -> Result<Option<Hit>, GraphError> {
    let client_id = cred.client_id.clone();
    let patterns = parse_code_patterns(&opts.code_regex);

    // Graph 车道并发闸：超出的调用方应该直接走 IMAP（不同服务端、独立限流）
    if !rate_acquire(Lane::Graph, &client_id, Duration::from_millis(500)).await {
        return Err(GraphError::Other("graph 车道已满，转 IMAP".into()));
    }

    let http = crate::providers::outlook::build_http_client(&cred.proxy)
        .map_err(|e| GraphError::Other(e.to_string()))?;

    let deadline = Instant::now() + opts.timeout;
    let mut bad_rounds = 0u32;
    let mut round = 0u32;

    while Instant::now() < deadline {
        round += 1;
        let round_start = Instant::now();

        // 每轮重新取 token（有缓存，命中不打 login 端点）
        let token = match access_token(&http, cred, GRAPH_SCOPE, email).await {
            Ok(t) => t,
            Err(e) => {
                let msg = e.to_string();
                if is_credential_fatal(&msg) {
                    return Err(GraphError::Fatal(msg));
                }
                // 网络/限流类：算一轮失败，继续下一轮
                bad_rounds += 1;
                if bad_rounds >= MAX_BAD_ROUNDS {
                    return Err(GraphError::Other(format!("token 连续失败: {msg}")));
                }
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };

        let mut round_ok = true;
        let mut found: Option<Hit> = None;

        for folder in GRAPH_FOLDERS {
            if Instant::now() >= deadline {
                break;
            }
            // Graph lane 令牌（每次 HTTP 请求一个）
            if !rate_acquire(Lane::Graph, &client_id, Duration::from_secs(2)).await {
                continue;
            }

            let since = opts
                .baseline_at
                .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));

            let res = with_hard_timeout(
                list_messages(&http, &token, folder, 10, since.as_deref()),
                HARD_TIMEOUT,
            )
            .await;

            match res {
                Ok(messages) => {
                    rate_report(Lane::Graph, &client_id, Outcome::Ok);
                    for msg in messages {
                        let mail = normalize_message(&msg);
                        if mail.message_id.is_empty() {
                            continue;
                        }
                        // 基线里已有的 id → 旧信，跳过
                        if opts.baseline_ids.contains(&mail.message_id) {
                            continue;
                        }
                        // 时间早于基线 → 旧信，跳过
                        if let Some(base) = opts.baseline_at {
                            if let Some(at) = normalize_utc(&mail.received_at) {
                                // 允许 60s 时钟误差
                                if at < base - chrono::Duration::seconds(60) {
                                    continue;
                                }
                            }
                        }

                        // 提取验证码：项目正则优先，再走通用规则
                        let code = extract_code_by_regex(
                            &format!("{}\n{}", mail.subject, mail.body),
                            &patterns,
                        );
                        let (code, from_regex) = if code.is_empty() {
                            (extract_code(&format!("{}\n{}", mail.subject, mail.body)), false)
                        } else {
                            (code, true)
                        };

                        // 跳过已返回过的码（同一封邮件重复轮询时）
                        if !code.is_empty() && opts.skip_codes.contains(&code) {
                            continue;
                        }

                        // 有码 → 成功；没码但有新邮件 → 交给调用方判断
                        // （「点击链接验证」型站点正文只有 URL，code 为空）
                        let mut hit_mail = mail;
                        hit_mail.folder = folder.to_string();
                        found = Some(Hit {
                            mail: hit_mail,
                            code,
                            from_regex,
                        });
                        break;
                    }
                }
                Err(GraphError::Throttled(code)) => {
                    // 429：退避后继续，**绝不 raise**
                    rate_report(Lane::Graph, &client_id, Outcome::Throttle);
                    rate_penalize(Lane::Graph, &client_id);
                    tracing::warn!("[graph] 429 限流，退避 2s 后继续 folder={folder}");
                    let _ = code;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    round_ok = false;
                }
                Err(GraphError::Server(code)) => {
                    rate_report(Lane::Graph, &client_id, Outcome::Throttle);
                    tracing::warn!("[graph] 5xx({code}) folder={folder}，本轮跳过");
                    round_ok = false;
                }
                Err(GraphError::Slow) => {
                    rate_report(Lane::Graph, &client_id, Outcome::Slow);
                    tracing::warn!("[graph] 响应变慢（硬超时）folder={folder}");
                    round_ok = false;
                }
                Err(GraphError::Fatal(msg)) => return Err(GraphError::Fatal(msg)),
                Err(e) => {
                    tracing::warn!("[graph] {e} folder={folder}");
                    round_ok = false;
                }
            }

            if found.is_some() {
                break;
            }
        }

        if let Some(hit) = found {
            return Ok(Some(hit));
        }

        // 整轮失败计数：连续 3 整轮才降级
        if round_ok {
            bad_rounds = 0;
        } else {
            bad_rounds += 1;
            if bad_rounds >= MAX_BAD_ROUNDS {
                return Err(GraphError::Other(format!(
                    "连续 {MAX_BAD_ROUNDS} 整轮失败，降级 IMAP"
                )));
            }
        }

        // 轮间等待：慢轮拉长间隔（避免越慢越打）
        let elapsed = round_start.elapsed();
        let sleep = if elapsed > Duration::from_secs(3) {
            SLOW_POLL_INTERVAL
        } else {
            POLL_INTERVAL
        };
        let remain = deadline.saturating_duration_since(Instant::now());
        let sleep = sleep.min(remain);
        if !sleep.is_zero() {
            tokio::time::sleep(sleep).await;
        }
    }

    tracing::info!("[graph] 轮询超时（{round} 轮）email={email}");
    Ok(None)
}

/// 快照收件箱基线：取当前 inbox 里所有 message id。
///
/// 取号瞬间调用。作用是挡掉回收号里的历史邮件 —— 实测号池账号收件箱
/// 普遍留着 8~14 天前的旧信，没有基线时 `receive` 会在 1.9 秒内
/// “成功”返回一个旧验证码，注册必然失败。
pub async fn snapshot_baseline(
    cred: &mut Credentials,
    email: &str,
) -> AppResult<(Vec<String>, chrono::DateTime<chrono::Utc>)> {
    let client_id = cred.client_id.clone();
    let http = crate::providers::outlook::build_http_client(&cred.proxy)?;

    let mut ids = Vec::new();
    let mut latest: Option<chrono::DateTime<chrono::Utc>> = None;

    // 抓基线同样过出口限流：并发取号时别和收码轮询抢配额
    if !rate_acquire(Lane::Graph, &client_id, Duration::from_secs(5)).await {
        return Err(AppError::bad_gateway("graph 车道繁忙，跳过基线"));
    }

    let token = access_token(&http, cred, GRAPH_SCOPE, email).await?;

    for folder in GRAPH_FOLDERS {
        if !rate_acquire(Lane::Graph, &client_id, Duration::from_secs(5)).await {
            continue;
        }
        match list_messages(&http, &token, folder, 50, None).await {
            Ok(messages) => {
                rate_report(Lane::Graph, &client_id, Outcome::Ok);
                for msg in messages {
                    if let Some(id) = msg.get("id").and_then(|v| v.as_str()) {
                        ids.push(id.to_string());
                    }
                    if let Some(at) = msg
                        .get("receivedDateTime")
                        .and_then(|v| v.as_str())
                        .and_then(normalize_utc)
                    {
                        latest = Some(match latest {
                            Some(prev) if prev >= at => prev,
                            _ => at,
                        });
                    }
                }
            }
            Err(e) => {
                tracing::warn!("[baseline] 抓 {folder} 失败（不阻断）: {e}");
            }
        }
    }

    Ok((ids, latest.unwrap_or_else(crate::util::utc_now)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_graph_message() {
        let msg = serde_json::json!({
            "id": "AAMk123",
            "subject": "Your verification code",
            "bodyPreview": "Your code is 123456",
            "receivedDateTime": "2026-11-22T01:23:45Z",
            "from": {"emailAddress": {"address": "noreply@service.com"}},
            "body": {"content": "<p>Your code is 123456</p>"}
        });
        let m = normalize_message(&msg);
        assert_eq!(m.message_id, "AAMk123");
        assert_eq!(m.sender, "noreply@service.com");
        assert_eq!(m.received_at, "2026-11-22T01:23:45Z");
        assert!(m.body.contains("123456"));
    }

    #[test]
    fn normalize_handles_missing_fields() {
        let m = normalize_message(&serde_json::json!({}));
        assert!(m.message_id.is_empty());
        assert!(m.sender.is_empty());
        assert!(m.body.is_empty());
    }

    #[test]
    fn graph_error_classification() {
        assert!(GraphError::Server(500).is_round_failure());
        assert!(GraphError::Other("x".into()).is_round_failure());
        assert!(!GraphError::Throttled(429).is_round_failure());
        assert!(GraphError::Throttled(429).is_throttle());
        assert!(GraphError::Server(503).is_throttle());
    }

    #[tokio::test]
    async fn hard_timeout_fires() {
        let r: Result<(), GraphError> = with_hard_timeout(
            async {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(())
            },
            Duration::from_millis(50),
        )
        .await;
        assert!(matches!(r, Err(GraphError::Slow)));
    }

    #[test]
    fn wait_options_default() {
        let o = WaitOptions::default();
        assert_eq!(o.timeout.as_secs(), 120);
        assert!(o.baseline_ids.is_empty());
        assert!(o.skip_codes.is_empty());
    }
}
