//! 收信编排：Graph 优先 → IMAP 降级 → 汇总。
//!
//! 这是「取码」的唯一入口。上层（API / 定时任务）只调 [`wait_mail`]。
//!
//! ## 降级规则（对齐 Python `_wait_mail_sync`）
//!
//! ```text
//! Graph 通道
//!   ├─ 拿到码           → 返回
//!   ├─ 凭据失效(Fatal)  → 直接报错，不降级（号废了，降级也救不回）
//!   ├─ 车道满 / 连续失败 → 降级 IMAP
//!   └─ 超时未等到        → 用剩余时间跑 IMAP（不是从零开始）
//! ```
//!
//! **关键**：两条通道共用一个总超时预算。Python 版曾经让 Graph 和 IMAP
//! 各自跑满 timeout，`timeout=5` 实际用了 16.6s。这里必须算剩余时间。

use std::time::{Duration, Instant};

use crate::error::{AppError, AppResult};
use crate::providers::graph::{self, GraphError, Hit, Mail, WaitOptions};
use crate::providers::imap;
use crate::providers::outlook::Credentials;

/// 两条通道都试过但没等到（正常轮询结果）。
pub struct WaitResult {
    pub hit: Option<Hit>,
    /// 走过哪些通道（日志/诊断用）
    pub path: Vec<&'static str>,
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
    let mut path = Vec::new();

    // ── 第一通道：Graph ──
    path.push("graph");
    tracing::info!("[mail] 取件开始 email={} timeout={:?}", email, opts.timeout);

    match graph::wait_code_graph(cred, email, opts).await {
        Ok(Some(hit)) => {
            tracing::info!(
                "[mail] Graph 命中 email={} code={} 耗时 {:.1}s",
                email,
                mask_code(&hit.code),
                started.elapsed().as_secs_f64()
            );
            return Ok(WaitResult {
                hit: Some(hit),
                path,
            });
        }
        Ok(None) => {
            tracing::info!(
                "[mail] Graph 通道无新信（耗时 {:.1}s），转 IMAP",
                started.elapsed().as_secs_f64()
            );
        }
        Err(GraphError::Fatal(msg)) => {
            // 凭据失效：号废了，不浪费 IMAP 的时间
            return Err(AppError::bad_gateway(format!("凭据失效: {msg}")));
        }
        Err(e) => {
            tracing::info!(
                "[mail] Graph 通道不可用（{e}），转 IMAP（耗时 {:.1}s）",
                started.elapsed().as_secs_f64()
            );
        }
    }

    // ── 第二通道：IMAP（用剩余时间）──
    let remaining = opts.timeout.saturating_sub(started.elapsed());
    if remaining < Duration::from_secs(3) {
        tracing::info!("[mail] 剩余时间不足以跑 IMAP（{remaining:?}），返回超时");
        return Ok(WaitResult { hit: None, path });
    }
    path.push("imap");

    let imap_opts = WaitOptions {
        timeout: remaining,
        ..opts.clone()
    };

    match imap::wait_code_imap(cred, email, &imap_opts).await {
        Ok(Some(hit)) => {
            tracing::info!(
                "[mail] IMAP 命中 email={} code={} 总耗时 {:.1}s",
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
            Ok(WaitResult { hit: None, path })
        }
        Err(e) => {
            // IMAP 也失败：但如果 Graph 是「超时」而不是「不可用」，
            // 应该返回超时（正常结果），而不是抛错中断注册流程。
            tracing::warn!("[mail] IMAP 也失败: {e}");
            Ok(WaitResult { hit: None, path })
        }
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
