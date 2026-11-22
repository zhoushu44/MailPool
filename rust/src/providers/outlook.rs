//! Outlook provider：Microsoft Graph 主通道 + IMAP XOAUTH2 降级通道。
//!
//! ## 为什么有两套通道
//!
//! Graph 快、能按 `$filter` 精确查时间范围，但**共用 client_id + 出口 IP**
//! 会被微软聚合限流；IMAP 走不同的服务端（`outlook.office365.com:993`、
//! `outlook.live.com:993`），配额独立。所以：
//! - Graph 车道同时轮询数有上限（`GRAPH_MAX_POLLERS`），超出直接走 IMAP
//! - 两条车道各自限流、互不干扰
//!
//! ## 限流策略（AIMD 自适应）
//!
//! 2026-11-22 并发实测：
//! ```text
//! 并发 15 → 15/15 成功，收码 ~30s（聚合 ~10 rps，正常）
//! 并发 20 → 12/20 成功，收码 229~236s（聚合 ~13 rps，雪崩）
//! ```
//! 微软限流的第一表现**不是 429 而是整体变慢**（单轮 4~8s）；继续加压才吐 429，
//! 而 429 一旦被轮询循环 `raise` 出去，该账号 Graph 通道当场死亡、整批挤进
//! IMAP 降级 → 雪崩到 230s。
//!
//! 治理：客户端把聚合速率压在实测安全线以下，超发排队等令牌。
//! 速率是 AIMD 自适应的（加性增、乘性减）：
//! - 连续 20 次成功 → `+0.5 rps`（探测真实上限）
//! - 429/503 → `×0.7` + 清空桶（微软动手了，全体停一拍）
//! - 硬超时 → `×0.9`（变慢是限流前兆，轻退一步）

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::util::{now_naive, truncate_chars};

// ────────────────────────────────────────────────────────────────
//  常量
// ────────────────────────────────────────────────────────────────

/// Graph 主作用域
pub const GRAPH_SCOPE: &str = "https://graph.microsoft.com/.default";
/// 同时支持 IMAP/SMTP 的 scope（Thunderbird 客户端 ID 用这个）
pub const IMAP_SCOPE: &str = "https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send offline_access";

pub const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

/// Token 端点（依次尝试，前一个挂了换下一个）
pub const TOKEN_ENDPOINTS: &[&str] = &[
    "https://login.microsoftonline.com/consumers/oauth2/v2.0/token",
    "https://login.microsoftonline.com/common/oauth2/v2.0/token",
    "https://login.live.com/oauth20_token.srf",
];

/// 查哪些文件夹（验证码几乎只落 inbox，但垃圾箱要兜底）
pub const GRAPH_FOLDERS: &[&str] = &["inbox", "junkemail"];

/// IMAP 服务器（依次尝试）
pub const IMAP_SERVERS: &[&str] = &["outlook.office365.com", "outlook.live.com"];

/// 邮件"新鲜度"默认窗口（分钟）
pub const MAX_AGE_MINUTES: i64 = 120;

// ────────────────────────────────────────────────────────────────
//  AIMD 令牌桶
// ────────────────────────────────────────────────────────────────

/// 连续成功多少次提速
const AIMD_GOOD_RUN: u32 = 20;
/// 每轮提速幅度
const AIMD_STEP: f64 = 0.5;
/// 429/503 退避系数
const AIMD_THROTTLE_X: f64 = 0.7;
/// 硬超时退避系数
const AIMD_SLOW_X: f64 = 0.9;

/// 请求结局（反馈给令牌桶调速）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 成功
    Ok,
    /// 429 / 503（微软明确限流）
    Throttle,
    /// 硬超时（响应变慢，限流前兆）
    Slow,
}

/// 线程安全令牌桶 + AIMD 自适应速率。
#[derive(Debug)]
pub struct TokenBucket {
    inner: Mutex<BucketState>,
    min_rate: f64,
    max_rate: f64,
    burst: f64,
}

#[derive(Debug)]
struct BucketState {
    rate: f64,
    tokens: f64,
    updated: Instant,
    good_run: u32,
}

impl TokenBucket {
    pub fn new(rate: f64, burst: f64, min_rate: f64, max_rate: f64) -> Self {
        let min_rate = min_rate.max(0.1);
        let max_rate = max_rate.max(min_rate);
        let rate = rate.clamp(min_rate, max_rate);
        Self {
            inner: Mutex::new(BucketState {
                rate,
                tokens: burst.max(1.0),
                updated: Instant::now(),
                good_run: 0,
            }),
            min_rate,
            max_rate,
            burst: burst.max(1.0),
        }
    }

    /// 当前速率（rps），测试用。
    pub fn rate(&self) -> f64 {
        self.inner.lock().unwrap().rate
    }

    /// 拿一个令牌；桶空时最多等 `max_wait` 秒。返回 false = 排队超时。
    ///
    /// 调用方应把超时当作「让位」而不是「失败」——不硬打，把配额留给别人。
    pub async fn acquire(&self, max_wait: Duration) -> bool {
        let deadline = Instant::now() + max_wait;
        loop {
            let wait_for = {
                let mut st = self.inner.lock().unwrap();
                let now = Instant::now();
                let elapsed = now.duration_since(st.updated).as_secs_f64();
                st.tokens = (st.tokens + elapsed * st.rate).min(self.burst);
                st.updated = now;

                if st.tokens >= 1.0 {
                    st.tokens -= 1.0;
                    return true;
                }
                let need = (1.0 - st.tokens) / st.rate;
                let left = deadline.saturating_duration_since(Instant::now());
                if need >= left.as_secs_f64() {
                    return false;
                }
                Duration::from_secs_f64(need)
            };
            tokio::time::sleep(wait_for).await;
        }
    }

    /// AIMD 调速入口。
    pub fn report(&self, outcome: Outcome) {
        let mut st = self.inner.lock().unwrap();
        match outcome {
            Outcome::Ok => {
                st.good_run += 1;
                if st.good_run >= AIMD_GOOD_RUN {
                    let old = st.rate;
                    st.rate = (st.rate + AIMD_STEP).min(self.max_rate);
                    if (st.rate - old).abs() > f64::EPSILON {
                        tracing::info!(
                            "[ratelimit] AIMD 提速 {old:.1} → {:.1} rps",
                            st.rate
                        );
                    }
                    st.good_run = 0;
                }
            }
            other => {
                st.good_run = 0;
                let factor = if other == Outcome::Throttle {
                    AIMD_THROTTLE_X
                } else {
                    AIMD_SLOW_X
                };
                let old = st.rate;
                st.rate = (st.rate * factor).max(self.min_rate);
                if (st.rate - old).abs() > f64::EPSILON {
                    tracing::info!(
                        "[ratelimit] AIMD 退避({other:?}) {old:.1} → {:.1} rps",
                        st.rate
                    );
                }
            }
        }
    }

    /// 收到 429/503 后清空桶：所有任务一起停一拍，别继续顶风加压。
    pub fn penalize(&self) {
        let mut st = self.inner.lock().unwrap();
        st.tokens = 0.0;
    }
}

// ────────────────────────────────────────────────────────────────
//  全局桶表（按 client_id 分组）
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    Graph,
    Token,
    Imap,
}

impl Lane {
    fn as_str(self) -> &'static str {
        match self {
            Lane::Graph => "graph",
            Lane::Token => "token",
            Lane::Imap => "imap",
        }
    }
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// 全局桶表：`(lane, client_id) → 桶`。
static BUCKETS: std::sync::OnceLock<Mutex<HashMap<(Lane, String), Arc<TokenBucket>>>> =
    std::sync::OnceLock::new();

fn buckets() -> &'static Mutex<HashMap<(Lane, String), Arc<TokenBucket>>> {
    BUCKETS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取（或创建）某个车道 + client_id 的令牌桶。
///
/// client_id 大小写不敏感（OAuth 的 GUID 不区分大小写）。
pub fn bucket_for(lane: Lane, client_id: &str) -> Arc<TokenBucket> {
    let key = (lane, client_id.trim().to_ascii_lowercase());
    let mut map = buckets().lock().unwrap();
    map.entry(key)
        .or_insert_with(|| {
            let (rate, burst, min, max) = match lane {
                Lane::Graph => (
                    env_f64("MAILPOOL_GRAPH_RPS", 8.0),
                    env_f64("MAILPOOL_GRAPH_BURST", 12.0),
                    env_f64("MAILPOOL_GRAPH_RPS_MIN", 3.0),
                    env_f64("MAILPOOL_GRAPH_RPS_MAX", 10.0),
                ),
                Lane::Token => (
                    env_f64("MAILPOOL_TOKEN_RPS", 4.0),
                    env_f64("MAILPOOL_TOKEN_BURST", 10.0),
                    env_f64("MAILPOOL_TOKEN_RPS_MIN", 2.0),
                    env_f64("MAILPOOL_TOKEN_RPS_MAX", 8.0),
                ),
                Lane::Imap => (
                    env_f64("MAILPOOL_IMAP_RPS", 6.0),
                    env_f64("MAILPOOL_IMAP_BURST", 10.0),
                    1.0,
                    20.0,
                ),
            };
            Arc::new(TokenBucket::new(rate, burst, min, max))
        })
        .clone()
}

/// 拿出口令牌；false = 排队超时（调用方应降速/让位，而不是硬打）。
pub async fn rate_acquire(lane: Lane, client_id: &str, max_wait: Duration) -> bool {
    bucket_for(lane, client_id).acquire(max_wait).await
}

/// 反馈结局（驱动 AIMD）。
pub fn rate_report(lane: Lane, client_id: &str, outcome: Outcome) {
    bucket_for(lane, client_id).report(outcome);
}

/// 429/503 后清空桶。
pub fn rate_penalize(lane: Lane, client_id: &str) {
    bucket_for(lane, client_id).penalize();
}

// ────────────────────────────────────────────────────────────────
//  access_token 缓存
// ────────────────────────────────────────────────────────────────

/// token 缓存提前量：提前 5 分钟算过期，避免边界上打空。
const TOKEN_TTL_MARGIN: i64 = 300;

#[derive(Debug, Clone)]
struct CachedToken {
    token: String,
    /// unix 秒
    expires_at: i64,
}

static TOKEN_CACHE: std::sync::OnceLock<Mutex<HashMap<String, CachedToken>>> =
    std::sync::OnceLock::new();

fn token_cache() -> &'static Mutex<HashMap<String, CachedToken>> {
    TOKEN_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_key(email: &str, scope: &str) -> String {
    format!("{scope}|{}", email.to_ascii_lowercase())
}

/// 缓存 access_token。
pub fn cache_token(email: &str, scope: &str, token: &str, expires_in: i64) {
    let expires_in = if expires_in <= 0 { 3600 } else { expires_in };
    let expires_at = crate::util::utc_now().timestamp() + expires_in - TOKEN_TTL_MARGIN;
    token_cache().lock().unwrap().insert(
        cache_key(email, scope),
        CachedToken {
            token: token.to_string(),
            expires_at,
        },
    );
}

/// 取缓存的 access_token（过期返回 None）。
pub fn cached_token(email: &str, scope: &str) -> Option<String> {
    let map = token_cache().lock().unwrap();
    let e = map.get(&cache_key(email, scope))?;
    if crate::util::utc_now().timestamp() >= e.expires_at {
        return None;
    }
    Some(e.token.clone())
}

/// 让某个 email+scope 的缓存失效（token 被判无效时调）。
pub fn invalidate_token(email: &str, scope: &str) {
    token_cache()
        .lock()
        .unwrap()
        .remove(&cache_key(email, scope));
}

// ────────────────────────────────────────────────────────────────
//  Retry-After 解析
// ────────────────────────────────────────────────────────────────

/// 从 `Retry-After` 头解析等待秒数。
///
/// 支持两种格式（RFC 7231）：
/// - 秒数：`Retry-After: 7`
/// - HTTP-date：`Retry-After: Wed, 21 Oct 2026 07:28:00 GMT`
pub fn retry_after_seconds(headers: &reqwest::header::HeaderMap) -> f64 {
    let Some(raw) = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
    else {
        return 0.0;
    };
    let raw = raw.trim();
    if let Ok(secs) = raw.parse::<f64>() {
        return secs.max(0.0);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(raw) {
        let now = crate::util::utc_now();
        let delta = (dt.with_timezone(&chrono::Utc) - now).num_seconds() as f64;
        return delta.max(0.0);
    }
    0.0
}

// ────────────────────────────────────────────────────────────────
//  凭据
// ────────────────────────────────────────────────────────────────

/// Outlook 账号凭据（存在 `email_accounts.account_json`）。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub email: String,
    /// SOCKS/HTTP 代理（可选）
    #[serde(default)]
    pub proxy: String,
    /// 其余字段原样保留（回写时不能丢）
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl Credentials {
    pub fn from_json(raw: &str) -> AppResult<Self> {
        serde_json::from_str(raw)
            .map_err(|e| AppError::bad_request(format!("凭据 JSON 解析失败: {e}")))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// 凭据是否够用（缺 client_id 或 refresh_token 就是废号）。
    pub fn is_complete(&self) -> bool {
        !self.client_id.trim().is_empty() && !self.refresh_token.trim().is_empty()
    }
}

// ────────────────────────────────────────────────────────────────
//  Token 刷新
// ────────────────────────────────────────────────────────────────

/// Token 刷新结果。
#[derive(Debug, Clone)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: i64,
}

/// 凭据失效的判定关键词（命中即认为号废了，不该重试）。
const FATAL_TOKEN_TOKENS: &[&str] = &[
    "invalid_grant",
    "invalid_client",
    "AADSTS70000",
    "AADSTS50173",
    "service abuse",
    "Basic authentication is disabled",
];

/// 错误是否属于「凭据确实失效」（而不是网络抖动）。
pub fn is_credential_fatal(msg: &str) -> bool {
    let low = msg.to_ascii_lowercase();
    FATAL_TOKEN_TOKENS
        .iter()
        .any(|t| low.contains(&t.to_ascii_lowercase()))
}

/// 风控（service abuse）判定。
pub fn is_risk_control(msg: &str) -> bool {
    let low = msg.to_ascii_lowercase();
    low.contains("service abuse") || low.contains("aadsts70000")
}

/// 刷新 access_token（带限流、重试、429 退避）。
///
/// 端点按 [`TOKEN_ENDPOINTS`] 依次尝试：某组 client_id 在 consumers 端点
/// 挂了不代表 common 也挂，全试过才放弃。
pub async fn refresh_token(
    http: &reqwest::Client,
    cred: &mut Credentials,
    scope: &str,
) -> AppResult<TokenResponse> {
    let client_id = cred.client_id.trim().to_string();
    if client_id.is_empty() {
        return Err(AppError::bad_request("缺少 client_id"));
    }
    if cred.refresh_token.trim().is_empty() {
        return Err(AppError::bad_request("缺少 refresh_token"));
    }

    let mut last_error = String::new();

    for (idx, endpoint) in TOKEN_ENDPOINTS.iter().enumerate() {
        // 出口限流：login 端点比 Graph 更容易被 client_id 级限流，
        // 批量刷新高峰时在这里排队，而不是一窝蜂打 429。
        if !rate_acquire(Lane::Token, &client_id, Duration::from_secs(8)).await {
            last_error = format!("token 令牌排队超时 {endpoint}");
            tracing::info!("[token] ✗ 端点 {}/{} 限流排队超时", idx + 1, TOKEN_ENDPOINTS.len());
            continue;
        }

        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let form = [
                ("grant_type", "refresh_token"),
                ("refresh_token", cred.refresh_token.as_str()),
                ("client_id", client_id.as_str()),
                ("scope", scope),
            ];

            let mut req = http.post(*endpoint).form(&form);
            if !cred.proxy.trim().is_empty() {
                // 代理在 Client 层配置（见 build_http_client），这里不重复设置
            }

            let resp = req.send().await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    let body: Value = match r.json().await {
                        Ok(v) => v,
                        Err(e) => {
                            last_error = format!("{endpoint} 响应非 JSON: {e}");
                            break;
                        }
                    };
                    let Some(at) = body.get("access_token").and_then(|v| v.as_str()) else {
                        last_error = format!("no access_token from {endpoint}");
                        tracing::info!(
                            "[token] ✗ 端点 {}/{} 返回无 access_token",
                            idx + 1,
                            TOKEN_ENDPOINTS.len()
                        );
                        break;
                    };
                    rate_report(Lane::Token, &client_id, Outcome::Ok);

                    // rt 轮换：微软会返回新的 refresh_token，旧的随即失效
                    let new_rt = body
                        .get("refresh_token")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    if let Some(rt) = &new_rt {
                        if rt != &cred.refresh_token {
                            cred.refresh_token = rt.clone();
                        }
                    }
                    let expires_in = body
                        .get("expires_in")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(3600);

                    tracing::info!(
                        "[token] ✓ 端点 {}/{} 刷新成功",
                        idx + 1,
                        TOKEN_ENDPOINTS.len()
                    );
                    return Ok(TokenResponse {
                        access_token: at.to_string(),
                        refresh_token: new_rt,
                        expires_in,
                    });
                }
                Ok(r) => {
                    let status = r.status();
                    let retry_after = retry_after_seconds(r.headers());
                    let text = r.text().await.unwrap_or_default();
                    last_error = format!("HTTP {status} {endpoint}: {text}");

                    // 429 / 5xx → 退避后重试（**不许 raise**：raise 会让整个
                    // 账号的 Graph 通道当场死亡，进而雪崩到 IMAP 降级）
                    if status.as_u16() == 429 || status.is_server_error() {
                        rate_report(Lane::Token, &client_id, Outcome::Throttle);
                        rate_penalize(Lane::Token, &client_id);
                        let wait = if retry_after > 0.0 {
                            retry_after
                        } else {
                            (2f64).powi(attempt.min(4) as i32)
                        };
                        if attempt >= 3 {
                            tracing::info!(
                                "[token] ✗ 端点 {}/{} HTTP {} 重试 {} 次仍失败",
                                idx + 1,
                                TOKEN_ENDPOINTS.len(),
                                status.as_u16(),
                                attempt
                            );
                            break;
                        }
                        tracing::warn!(
                            "[token] 端点 {} HTTP {}（限流），歇 {:.0}s 后重试（第 {} 次）",
                            idx + 1,
                            status.as_u16(),
                            wait,
                            attempt
                        );
                        tokio::time::sleep(Duration::from_secs_f64(wait.min(15.0))).await;
                        continue;
                    }

                    // 400/401/403：这组 client_id 在这个端点不可用，换下一个端点
                    if matches!(status.as_u16(), 400 | 401 | 403) {
                        tracing::info!(
                            "[token] ✗ 端点 {}/{} HTTP {}: {}",
                            idx + 1,
                            TOKEN_ENDPOINTS.len(),
                            status.as_u16(),
                            truncate_chars(&text, 80)
                        );
                        break;
                    }
                    break;
                }
                Err(e) => {
                    last_error = format!("{endpoint}: {e}");
                    if e.is_timeout() {
                        rate_report(Lane::Token, &client_id, Outcome::Slow);
                    }
                    tracing::info!(
                        "[token] ✗ 端点 {}/{} 异常: {}",
                        idx + 1,
                        TOKEN_ENDPOINTS.len(),
                        e
                    );
                    break;
                }
            }
        }
    }

    Err(AppError::bad_gateway(format!(
        "所有端点均失败，scope={scope} last_error={last_error}"
    )))
}

/// 带缓存的取 token：缓存命中直接返回，不重复打 login 端点。
///
/// （实测：一次 `wait_code` 会刷新 2~3 次 token，缓存后降到 0~1 次）
pub async fn access_token(
    http: &reqwest::Client,
    cred: &mut Credentials,
    scope: &str,
    email: &str,
) -> AppResult<String> {
    if let Some(t) = cached_token(email, scope) {
        return Ok(t);
    }
    let data = refresh_token(http, cred, scope).await?;
    cache_token(email, scope, &data.access_token, data.expires_in);
    Ok(data.access_token)
}

// ────────────────────────────────────────────────────────────────
//  HTTP 客户端
// ────────────────────────────────────────────────────────────────

/// 构造 HTTP 客户端（超时 + 连接复用 + 可选 SOCKS 代理）。
pub fn build_http_client(proxy: &str) -> AppResult<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(16);
    if !proxy.trim().is_empty() {
        let p = reqwest::Proxy::all(proxy.trim())
            .map_err(|e| AppError::bad_request(format!("代理地址无效: {e}")))?;
        b = b.proxy(p);
    }
    b.build()
        .map_err(|e| AppError::internal(format!("构建 HTTP 客户端失败: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_respects_burst() {
        let b = TokenBucket::new(10.0, 3.0, 1.0, 20.0);
        // 同步拿 3 个（burst），第 4 个必须排队
        assert!(futures_block_on(b.acquire(Duration::ZERO)));
        assert!(futures_block_on(b.acquire(Duration::ZERO)));
        assert!(futures_block_on(b.acquire(Duration::ZERO)));
        assert!(!futures_block_on(b.acquire(Duration::from_millis(1))));
    }

    /// 极简 block_on（避免引 tokio 测试运行时到单测）。
    fn futures_block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn buckets_keyed_by_lane_and_client_id() {
        let a = bucket_for(Lane::Graph, "cid-a");
        let b = bucket_for(Lane::Graph, "CID-A"); // 大小写不敏感 → 同一个
        assert!(Arc::ptr_eq(&a, &b));

        let c = bucket_for(Lane::Graph, "cid-b");
        assert!(!Arc::ptr_eq(&a, &c));

        let d = bucket_for(Lane::Token, "cid-a"); // 不同车道 → 不同桶
        assert!(!Arc::ptr_eq(&a, &d));
    }

    #[test]
    fn aimd_speeds_up_after_good_run() {
        let b = TokenBucket::new(5.0, 5.0, 1.0, 10.0);
        let before = b.rate();
        for _ in 0..AIMD_GOOD_RUN {
            b.report(Outcome::Ok);
        }
        assert!(
            b.rate() > before,
            "连续 {} 次成功后应提速: {} → {}",
            AIMD_GOOD_RUN,
            before,
            b.rate()
        );
    }

    #[test]
    fn aimd_backs_off_on_throttle() {
        let b = TokenBucket::new(8.0, 8.0, 1.0, 10.0);
        let before = b.rate();
        b.report(Outcome::Throttle);
        assert!(b.rate() < before, "429 后应退避: {before} → {}", b.rate());
        // 不能低于下限
        for _ in 0..50 {
            b.report(Outcome::Throttle);
        }
        assert!(b.rate() >= 1.0, "速率不得低于下限: {}", b.rate());
    }

    #[test]
    fn penalty_drains_bucket() {
        let b = TokenBucket::new(1.0, 5.0, 1.0, 10.0);
        b.penalize();
        assert!(!futures_block_on(b.acquire(Duration::from_millis(50))));
    }

    #[test]
    fn retry_after_seconds_value() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::RETRY_AFTER, "7".parse().unwrap());
        assert_eq!(retry_after_seconds(&h), 7.0);
    }

    #[test]
    fn retry_after_missing_header() {
        let h = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after_seconds(&h), 0.0);
    }

    #[test]
    fn retry_after_http_date() {
        let mut h = reqwest::header::HeaderMap::new();
        let when = crate::util::utc_now() + chrono::Duration::seconds(30);
        h.insert(
            reqwest::header::RETRY_AFTER,
            when.format("%a, %d %b %Y %H:%M:%S GMT").to_string().parse().unwrap(),
        );
        let v = retry_after_seconds(&h);
        assert!((25.0..=31.0).contains(&v), "解析出 {v}");
    }

    #[test]
    fn token_cache_ttl_margin() {
        cache_token("ttl@test", GRAPH_SCOPE, "tok", 3600);
        assert_eq!(cached_token("ttl@test", GRAPH_SCOPE).as_deref(), Some("tok"));
        // 提前 300s 过期 → 剩余约 3300s
        let map = token_cache().lock().unwrap();
        let e = map.get(&cache_key("ttl@test", GRAPH_SCOPE)).unwrap();
        let remaining = e.expires_at - crate::util::utc_now().timestamp();
        assert!(
            (3200..=3305).contains(&remaining),
            "剩余 {remaining}s，应约 3300s（3600 - 300 提前量）"
        );
    }

    #[test]
    fn token_cache_bad_expires_in() {
        // expires_in 传垃圾值 → 回退 3600，不能变成已过期
        cache_token("bad@test", GRAPH_SCOPE, "tok2", 0);
        assert_eq!(cached_token("bad@test", GRAPH_SCOPE).as_deref(), Some("tok2"));
    }

    #[test]
    fn invalidate_token_works() {
        cache_token("inv@test", GRAPH_SCOPE, "t", 3600);
        assert!(cached_token("inv@test", GRAPH_SCOPE).is_some());
        invalidate_token("inv@test", GRAPH_SCOPE);
        assert!(cached_token("inv@test", GRAPH_SCOPE).is_none());
    }

    #[test]
    fn credential_fatal_detection() {
        assert!(is_credential_fatal("invalid_grant: token expired"));
        assert!(is_credential_fatal("AADSTS70000"));
        assert!(is_credential_fatal("service abuse mode"));
        assert!(!is_credential_fatal("connection timeout"));
        assert!(!is_credential_fatal("HTTP 429"));
    }

    #[test]
    fn risk_control_detection() {
        assert!(is_risk_control("account is in service abuse mode"));
        assert!(is_risk_control("AADSTS70000"));
        assert!(!is_risk_control("invalid_grant"));
    }

    #[test]
    fn credentials_roundtrip_preserves_extra() {
        let raw = r#"{"client_id":"c","refresh_token":"r","email":"a@b.test","custom":"keepme"}"#;
        let mut c = Credentials::from_json(raw).unwrap();
        assert!(c.is_complete());
        assert_eq!(c.extra.get("custom").unwrap(), "keepme");
        // 改 rt 后回写，自定义字段不能丢
        c.refresh_token = "new".into();
        let out = c.to_json();
        assert!(out.contains("keepme"), "自定义字段被丢了: {out}");
        assert!(out.contains("\"new\""));
    }

    #[test]
    fn credentials_incomplete() {
        let c = Credentials::from_json(r#"{"client_id":"","refresh_token":"r"}"#).unwrap();
        assert!(!c.is_complete());
    }
}
