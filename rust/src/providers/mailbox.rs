//! 收信编排：Graph 优先 → IMAP 降级 → 汇总。
//!
//! 这是「取码」的唯一入口。上层（API / 定时任务）只调 [`wait_mail`]。
//!
//! ## 路由策略（2026-11-22 对齐 Python 版）：**IMAP 优先，Graph 兜底**
//!
//! ```text
//! 取号 → 有 OAuth 凭据？
//!   ├─ 无 → IMAP 直接 fatal（basic auth 被微软禁了，本来就没路）
//!   ├─ 已标记无 IMAP scope → Graph 直连（令牌桶排队，不会雪崩）
//!   └─ 未标记 → IMAP 优先，整个等待窗口给 IMAP
//!         ├─ 命中 → 返回验证码
//!         ├─ 干净跑完没新信 → 返回 None（INBOX/junk 都扫了，Graph 重扫也是空）
//!         └─ 失败 → 记进无 scope 名单 + 剩余预算走 Graph 兜底（永不判死）
//! ```
//!
//! ### 为什么 IMAP 做主车道
//!
//! - IMAP 走 `outlook.office365.com:993`，**没有 client_id 级聚合限流**
//!   （基准实测 21 并发满速零错误）
//! - Graph 单 client_id 聚合 ~10 rps 封顶，只留给无 IMAP scope 的号和兜底
//!
//! ### 关键：三种结果必须区分清楚
//!
//! 这是移植时最容易做错的地方。IMAP 车道跑完可能返回三种状态：
//!
//! | 结果 | 含义 | 后续动作 |
//! |---|---|---|
//! | `Hit` | 拿到码 | 返回 |
//! | `Clean` | 干净超时（扫完了，就是没新信） | **直接返回 None，不要再跑 Graph** |
//! | `Failed` | 通道本身坏了（无 scope / 认证失败 / 网络） | 标记无 scope + Graph 兜底 |
//!
//! 如果不区分 `Clean` 和 `Failed`，要么会把「没新信」误判成「无 scope」
//! 污染名单，要么会对同一个邮箱白扫一遍 Graph。
//!
//! ### 踩过的坑
//!
//! 1. 原来两条通道各自跑满 timeout → `timeout=5` 实际用了 16.6 秒
//! 2. 一条通道干净跑完（只是没新信）后不再用另一条重扫同一邮箱

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::error::{AppError, AppResult};
use crate::providers::graph::{self, GraphError, Hit, Mail, WaitOptions};
use crate::providers::imap;
use crate::providers::outlook::Credentials;

/// 已知没有 IMAP scope 的账号（邮箱小写）。
///
/// 第一次给某号刷 IMAP token 被微软拒绝时记进来，之后这个号直接走 Graph，
/// 不再浪费一次注定失败的刷新（省 1~2 秒）。
///
/// **只放内存**（对齐 Python `_imap_no_scope`）：重启后重新学一遍。
/// 放进数据库要多一张表 + 清理逻辑，而重新学习的代价只是每个号多花 1~2 秒一次。
static IMAP_NO_SCOPE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn no_scope_set() -> &'static Mutex<HashSet<String>> {
    IMAP_NO_SCOPE.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 该邮箱是否已被标记为「无 IMAP scope」。
pub fn is_no_imap_scope(email: &str) -> bool {
    no_scope_set()
        .lock()
        .unwrap()
        .contains(&email.trim().to_ascii_lowercase())
}

/// 标记为「无 IMAP scope」。
pub fn mark_no_imap_scope(email: &str) {
    no_scope_set()
        .lock()
        .unwrap()
        .insert(email.trim().to_ascii_lowercase());
}

/// 清空名单（测试用）。
#[cfg(test)]
pub fn clear_no_imap_scope() {
    no_scope_set().lock().unwrap().clear();
}

/// IMAP 车道的三种结局。
enum ImapOutcome {
    /// 拿到码
    Hit(Box<Hit>),
    /// 干净超时：扫完了就是没新信
    Clean,
    /// 通道失败（无 scope / 认证失败 / 网络），可以去 Graph 兜底
    Failed(String),
}

/// 两条通道都试过但没等到（正常轮询结果）。
pub struct WaitResult {
    pub hit: Option<Hit>,
    /// 走过哪些通道（日志/诊断用）
    pub path: Vec<&'static str>,
}

impl WaitResult {
    fn none(mut path: Vec<&'static str>) -> Self {
        path.shrink_to_fit();
        Self { hit: None, path }
    }
}

/// 等一封验证码邮件。
///
/// 返回 `Ok(hit=None)` = 超时（**调用方应当返回 status=timeout，不是错误**）。
/// 返回 `Err` = 凭据失效或两条通道都不可用。
pub async fn wait_mail(
    cred: &mut Credentials,
    email: &str,
    opts: &WaitOptions,
) -> AppResult<WaitResult> {
    let started = Instant::now();
    let mut path: Vec<&'static str> = Vec::new();
    tracing::info!("[mail] 取件开始 email={email} timeout={:?}", opts.timeout);

    // ── 前置：有 OAuth 凭据吗 ──
    if !cred.is_complete() {
        // 无 OAuth 凭据：IMAP 直接 fatal（basic auth 已被微软禁用，无路可走）
        path.push("imap");
        let err = imap::wait_code_imap(cred, email, opts).await.err();
        return Err(err.unwrap_or_else(|| {
            AppError::bad_gateway(format!("凭据无效且 IMAP 不可用: {email}"))
        }));
    }

    // ── 已标记无 IMAP scope → Graph 直连 ──
    if is_no_imap_scope(email) {
        path.push("graph");
        tracing::debug!("[mail] {email} 已标记无 IMAP scope，直接走 Graph");
        return match graph::wait_code_graph(cred, email, opts).await {
            Ok(Some(hit)) => Ok(WaitResult {
                hit: Some(hit),
                path,
            }),
            Ok(None) => Ok(WaitResult::none(path)),
            Err(GraphError::Fatal(msg)) => {
                Err(AppError::bad_gateway(format!("凭据失效: {msg}")))
            }
            Err(e) => {
                tracing::warn!("[mail] Graph 直连失败: {e}");
                Ok(WaitResult::none(path))
            }
        };
    }

    // ── IMAP 优先：整个等待窗口给 IMAP ──
    path.push("imap");
    let outcome = run_imap_lane(cred, email, opts).await;

    match outcome {
        ImapOutcome::Hit(hit) => {
            tracing::info!(
                "[mail] IMAP 命中 email={} code={} 耗时 {:.1}s",
                email,
                mask_code(&hit.code),
                started.elapsed().as_secs_f64()
            );
            return Ok(WaitResult {
                hit: Some(*hit),
                path,
            });
        }
        ImapOutcome::Clean => {
            // 干净跑完没新信：INBOX/junk 都扫过了，Graph 重扫也是空。
            // **不再跑 Graph** —— 这正是「两条通道各自跑满 timeout」的坑。
            tracing::info!(
                "[mail] IMAP 车道无新信（耗时 {:.1}s）",
                started.elapsed().as_secs_f64()
            );
            return Ok(WaitResult::none(path));
        }
        ImapOutcome::Failed(reason) => {
            // 首次失败即记忆（保守：瞬时网络抖动也会被记，代价只是此号
            // 以后走 Graph 排队；换来的是不会再烧一次注定失败的刷新）
            mark_no_imap_scope(email);
            tracing::info!(
                "[mail] IMAP 车道失败，标记无 scope 并退回 Graph 兜底：{}",
                crate::util::truncate_chars(&reason, 80)
            );
        }
    }

    // ── Graph 兜底：用剩余预算 ──
    let remaining = opts.timeout.saturating_sub(started.elapsed());
    if remaining < Duration::from_secs(1) {
        tracing::info!("[mail] 时间预算已用尽，返回 None");
        return Ok(WaitResult::none(path));
    }

    path.push("graph");
    let graph_opts = WaitOptions {
        timeout: remaining,
        ..opts.clone()
    };

    match graph::wait_code_graph(cred, email, &graph_opts).await {
        Ok(Some(hit)) => {
            tracing::info!(
                "[mail] Graph 兜底命中 email={} code={} 总耗时 {:.1}s",
                email,
                mask_code(&hit.code),
                started.elapsed().as_secs_f64()
            );
            Ok(WaitResult {
                hit: Some(hit),
                path,
            })
        }
        Ok(None) => {
            tracing::info!(
                "[mail] 两条通道都没等到新信 email={} 总耗时 {:.1}s",
                email,
                started.elapsed().as_secs_f64()
            );
            Ok(WaitResult::none(path))
        }
        Err(GraphError::Fatal(msg)) => {
            Err(AppError::bad_gateway(format!("凭据失效: {msg}")))
        }
        Err(e) => {
            tracing::warn!("[mail] Graph 兜底也失败: {e}");
            Ok(WaitResult::none(path))
        }
    }
}

/// 跑 IMAP 车道，并把 `AppResult` 归一成「可以决定下一步」的三态。
///
/// **关键**：`wait_code_imap` 的错误被吞成 `Failed`，因为 IMAP 失败不代表
/// 这个号废了（可能只是没 scope），必须能去 Graph 兜底。
/// 但「干净返回 None」要保留为 `Clean`，否则会对同一邮箱白扫一遍 Graph。
async fn run_imap_lane(
    cred: &mut Credentials,
    email: &str,
    opts: &WaitOptions,
) -> ImapOutcome {
    match imap::wait_code_imap(cred, email, opts).await {
        Ok(Some(hit)) => ImapOutcome::Hit(Box::new(hit)),
        Ok(None) => ImapOutcome::Clean,
        Err(e) => ImapOutcome::Failed(e.to_string()),
    }
}

/// 预检：刷新一次 token 判断号还活着没。
///
/// 返回 `(alive, reason)`。网络抖动等临时错误视为 `alive=true`
/// （不放死号进池，但也不因为一次网络问题就误杀号）。
pub async fn probe_account(cred: &mut Credentials, email: &str) -> (bool, String) {
    if !cred.is_complete() {
        return (false, "credential_invalid: 缺 client_id 或 refresh_token".into());
    }
    imap::verify_account(cred, email).await
}

/// 快照基线（取号瞬间）。
pub async fn snapshot_baseline(
    cred: &mut Credentials,
    email: &str,
) -> AppResult<(Vec<String>, chrono::DateTime<chrono::Utc>)> {
    graph::snapshot_baseline(cred, email).await
}

/// 日志里不打印完整验证码（防日志泄露）。
fn mask_code(code: &str) -> String {
    if code.is_empty() {
        return "(空)".into();
    }
    if code.len() <= 2 {
        return "*".repeat(code.len());
    }
    format!("{}**{}", &code[..1], &code[code.len() - 1..])
}

/// 把 [`Hit`] 转成对外的 mail 结构（API 的 `mails[]` 字段）。
pub fn hit_to_api_mail(hit: &Hit) -> serde_json::Value {
    serde_json::json!({
        "id": hit.mail.message_id,
        "message_id": hit.mail.message_id,
        "subject": hit.mail.subject,
        "body": hit.mail.body,
        "body_preview": hit.mail.body_preview,
        "from": hit.mail.sender,
        "sender": hit.mail.sender,
        "folder": hit.mail.folder,
        "received_at": crate::util::iso(&hit.mail.received_at),
    })
}

/// 统一取一封邮件（管理端收件箱用，不轮询）。
pub async fn fetch_messages(
    cred: &mut Credentials,
    email: &str,
    folder: &str,
    top: u32,
) -> AppResult<Vec<Mail>> {
    let http = crate::providers::outlook::build_http_client(&cred.proxy)?;
    let token = crate::providers::outlook::access_token(
        &http,
        cred,
        crate::providers::outlook::GRAPH_SCOPE,
        email,
    )
    .await?;
    let msgs = graph::list_messages(&http, &token, folder, top, None)
        .await
        .map_err(|e| AppError::bad_gateway(format!("Graph 获取邮件失败: {e}")))?;
    Ok(msgs
        .iter()
        .map(|m| {
            let mut mail = graph::normalize_message(m);
            mail.folder = folder.to_string();
            mail
        })
        .collect())
}

#[cfg(test)]
mod tests {

    /// 操作全局 `IMAP_NO_SCOPE` 的测试必须串行。
    ///
    /// 这些测试共享同一个进程级 `static`，cargo 默认多线程并行跑测试，
    /// 一个测试里的 `clear_no_imap_scope()` 会把另一个测试刚标记的条目抹掉，
    /// 造成随机失败（实测：`no_scope_mark_is_case_insensitive` 偶发挂）。
    ///
    /// 用一把全局锁把这类测试串起来，比改成 `#[serial]` 少引一个 crate。
    static NO_SCOPE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_no_scope() -> std::sync::MutexGuard<'static, ()> {
        NO_SCOPE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ────────────────────────────────────────────────────────────
    //  路由语义回归（对齐 Python TestDualLane 的 4 个用例）
    // ────────────────────────────────────────────────────────────

    #[test]
    fn no_scope_mark_is_case_insensitive() {
        let _g = lock_no_scope();
        clear_no_imap_scope();
        mark_no_imap_scope("Foo@Bar.TEST");
        assert!(is_no_imap_scope("foo@bar.test"));
        assert!(is_no_imap_scope("  FOO@BAR.TEST  "));
        assert!(!is_no_imap_scope("other@bar.test"));
        clear_no_imap_scope();
    }

    #[test]
    fn no_scope_does_not_leak_between_accounts() {
        let _g = lock_no_scope();
        clear_no_imap_scope();
        mark_no_imap_scope("a@test.com");
        assert!(!is_no_imap_scope("b@test.com"), "标记不能串号");
        clear_no_imap_scope();
    }

    #[tokio::test]
    async fn no_oauth_credentials_goes_imap_first_and_errors() {
        // 无 OAuth 凭据 → 直接返回 Err（IMAP 也救不了：basic auth 被禁用）
        clear_no_imap_scope();
        let mut cred = Credentials::default(); // 空凭据
        let opts = WaitOptions {
            timeout: Duration::from_millis(200),
            ..Default::default()
        };
        let r = wait_mail(&mut cred, "nocred@test.com", &opts).await;
        assert!(r.is_err(), "无凭据应返回错误，实际 {r:?}", r = r.map(|x| x.hit.is_some()));
    }

    #[test]
    fn imap_outcome_three_states_are_distinct() {
        // 这是个「契约测试」：三种结局必须是三个不同分支。
        // 如果将来有人把 Clean 和 Failed 合并，这个测试的编译就会失败
        // （因为 match 会不完整）。
        fn classify(o: &ImapOutcome) -> &'static str {
            match o {
                ImapOutcome::Hit(_) => "hit",
                ImapOutcome::Clean => "clean",
                ImapOutcome::Failed(_) => "failed",
            }
        }
        assert_eq!(classify(&ImapOutcome::Clean), "clean");
        assert_eq!(classify(&ImapOutcome::Failed("x".into())), "failed");
    }

    #[test]
    fn wait_result_none_has_empty_hit() {
        let r = WaitResult::none(vec!["imap"]);
        assert!(r.hit.is_none());
        assert_eq!(r.path, vec!["imap"]);
    }
    use super::*;

    #[test]
    fn mask_code_hides_middle() {
        assert_eq!(mask_code("123456"), "1**6");
        assert_eq!(mask_code("12"), "**");
        assert_eq!(mask_code("1"), "*");
        assert_eq!(mask_code(""), "(空)");
    }

    #[test]
    fn hit_to_api_mail_has_all_fields() {
        let hit = Hit {
            mail: Mail {
                message_id: "id1".into(),
                subject: "subj".into(),
                body: "body".into(),
                body_preview: "prev".into(),
                sender: "a@b.test".into(),
                folder: "inbox".into(),
                received_at: "2026-11-22T01:00:00Z".into(),
                provider: "outlook".into(),
            },
            code: "123456".into(),
            from_regex: false,
        };
        let v = hit_to_api_mail(&hit);
        // 客户端契约：from 与 sender 都要有（有的客户端读 from，有的读 sender）
        assert_eq!(v["from"], "a@b.test");
        assert_eq!(v["sender"], "a@b.test");
        assert_eq!(v["message_id"], "id1");
        assert_eq!(v["id"], "id1");
    }
}
